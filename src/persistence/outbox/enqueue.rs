use crate::error::AppError;
use rusqlite::{Transaction, params};

use super::{
    DeliveryRecord, EnqueueDelivery, MAX_QUEUE_GLOBAL, MAX_QUEUE_TENANT, find_event, get, hash,
    internal, validate, validate_dependency_contract,
};

pub fn enqueue_many_in_transaction(
    tx: &Transaction<'_>,
    inputs: &[(EnqueueDelivery, String)],
    now: i64,
) -> Result<Vec<DeliveryRecord>, AppError> {
    for (input, _) in inputs {
        validate(input)?;
        if let Some(supplied) = &input.payload_sha256
            && supplied != &hash(&input.payload)
        {
            return Err(AppError::Validation(
                "payload hash does not match payload".into(),
            ));
        }
    }
    let mut records = Vec::with_capacity(inputs.len());
    let mut fresh = Vec::new();
    for (input, id) in inputs {
        if let Some(row) = find_event(tx, input)? {
            if row.payload_sha256 != hash(&input.payload)
                || row.event_type != input.event_type
                || row.secret_ref != input.secret_ref
                || row.durability_intent_id != input.durability_intent_id
                || row.delivery_kind != input.delivery_kind
                || row.ordering_key != input.ordering_key
                || row.depends_on_outbox_id != input.depends_on_outbox_id
            {
                return Err(AppError::Conflict("outbox idempotency conflict".into()));
            }
            records.push(row);
        } else {
            fresh.push((input, id));
        }
    }
    let active: i64 = tx.query_row("SELECT COUNT(*) FROM provider_delivery_outbox WHERE state IN ('blocked', 'ready', 'leased', 'retry')", [], |r| r.get(0)).map_err(internal)?;
    if active + i64::try_from(fresh.len()).unwrap_or(i64::MAX) > MAX_QUEUE_GLOBAL {
        return Err(AppError::RateLimited);
    }
    for tenant in fresh
        .iter()
        .map(|(i, _)| &i.tenant)
        .collect::<std::collections::BTreeSet<_>>()
    {
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM provider_delivery_outbox WHERE tenant = ?1 AND state IN ('blocked', 'ready', 'leased', 'retry')", [tenant], |r| r.get(0)).map_err(internal)?;
        let additions = fresh.iter().filter(|(i, _)| &i.tenant == tenant).count();
        if count + i64::try_from(additions).unwrap_or(i64::MAX) > MAX_QUEUE_TENANT {
            return Err(AppError::RateLimited);
        }
    }
    for (input, id) in fresh {
        validate_dependency_contract(tx, input)?;
        let state = if input.durability_intent_id.is_some() {
            "blocked"
        } else {
            "ready"
        };
        let prior_created_at: Option<i64> = tx
            .query_row(
                "SELECT MAX(created_at) FROM provider_delivery_outbox WHERE ordering_key = ?1",
                [&input.ordering_key],
                |row| row.get(0),
            )
            .map_err(internal)?;
        let created_at = prior_created_at.map_or(Ok(now), |prior| {
            prior
                .checked_add(1)
                .map(|next| now.max(next))
                .ok_or(AppError::Internal)
        })?;
        let digest = hash(&input.payload);
        tx.execute("INSERT INTO provider_delivery_outbox (id, provider, event_id, tenant, event_type, target_key, bucket_id, secret_ref, payload, payload_sha256, durability_intent_id, delivery_kind, ordering_key, depends_on_outbox_id, state, next_attempt_at, created_at, updated_at) VALUES (?1, 'discord', ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?14)", params![id, input.event_id, input.tenant, input.event_type, input.target_key, input.secret_ref, input.payload, digest, input.durability_intent_id, input.delivery_kind, input.ordering_key, input.depends_on_outbox_id, state, now, created_at]).map_err(internal)?;
        records.push(get(tx, id)?.ok_or(AppError::Internal)?);
    }
    Ok(records)
}
pub fn enqueue_in_transaction(
    tx: &Transaction<'_>,
    input: &EnqueueDelivery,
    id: &str,
    now: i64,
) -> Result<DeliveryRecord, AppError> {
    enqueue_many_in_transaction(tx, &[(input.clone(), id.to_owned())], now)
        .map(|mut rows| rows.remove(0))
}

/// Producer success: release every matching blocked row, clear its FK, then delete the intent.
pub fn finalize_durability_success_in_transaction(
    tx: &Transaction<'_>,
    intent: &str,
    now: i64,
) -> Result<usize, AppError> {
    let changed = tx.execute("UPDATE provider_delivery_outbox SET state = 'ready', durability_intent_id = NULL, updated_at = ?1 WHERE durability_intent_id = ?2 AND state = 'blocked'", params![now, intent]).map_err(internal)?;
    if tx
        .execute(
            "DELETE FROM artifact_durability_intents WHERE id = ?1",
            [intent],
        )
        .map_err(internal)?
        != 1
    {
        return Err(AppError::Internal);
    }
    Ok(changed)
}
/// Producer compensation: remove blocked outbox work first so the restricted intent FK stays safe.
pub fn compensate_durability_in_transaction(
    tx: &Transaction<'_>,
    intent: &str,
) -> Result<usize, AppError> {
    let changed = tx.execute("DELETE FROM provider_delivery_outbox WHERE durability_intent_id = ?1 AND state = 'blocked'", [intent]).map_err(internal)?;
    if tx
        .execute(
            "DELETE FROM artifact_durability_intents WHERE id = ?1",
            [intent],
        )
        .map_err(internal)?
        != 1
    {
        return Err(AppError::Internal);
    }
    Ok(changed)
}
