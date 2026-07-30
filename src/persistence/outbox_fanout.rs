//! Secret-free webhook subscriber query and all-or-none outbox fanout.

use crate::{
    error::AppError,
    integrations::delivery_envelope::DeliveryEnvelopeV1,
    model::{OrgId, WebhookEvent},
    persistence::{
        outbox::{self, DeliveryRecord, EnqueueDelivery},
        webhooks::event_name,
    },
};
use rusqlite::{Transaction, params};

/// Fan out one canonical envelope to the subscribers that are configured at transaction time.
/// This reads only `id`/`events`; it cannot decrypt, select, or expose webhook URLs.
pub fn fanout_in_transaction<F>(
    tx: &Transaction<'_>,
    envelope: &DeliveryEnvelopeV1,
    tenant: &OrgId,
    event: &WebhookEvent,
    durability_intent_id: Option<String>,
    now: i64,
    next_id: F,
) -> Result<Vec<DeliveryRecord>, AppError>
where
    F: FnMut() -> Result<String, AppError>,
{
    fanout_with_policy(
        tx,
        envelope,
        tenant,
        event,
        FanoutPolicy {
            durability_intent_id,
            excluded_target: None,
        },
        now,
        next_id,
    )
}

/// Fan out while omitting one explicit target. Notification-anchored discussions use this only
/// for the selected webhook's feedback/resolved events, because the same content is posted into
/// its artifact thread instead of duplicated in the parent channel.
pub fn fanout_in_transaction_excluding<F>(
    tx: &Transaction<'_>,
    envelope: &DeliveryEnvelopeV1,
    tenant: &OrgId,
    event: &WebhookEvent,
    now: i64,
    excluded_target: Option<&str>,
    next_id: F,
) -> Result<Vec<DeliveryRecord>, AppError>
where
    F: FnMut() -> Result<String, AppError>,
{
    fanout_with_policy(
        tx,
        envelope,
        tenant,
        event,
        FanoutPolicy {
            durability_intent_id: None,
            excluded_target,
        },
        now,
        next_id,
    )
}

struct FanoutPolicy<'a> {
    durability_intent_id: Option<String>,
    excluded_target: Option<&'a str>,
}

fn fanout_with_policy<F>(
    tx: &Transaction<'_>,
    envelope: &DeliveryEnvelopeV1,
    tenant: &OrgId,
    event: &WebhookEvent,
    policy: FanoutPolicy<'_>,
    now: i64,
    mut next_id: F,
) -> Result<Vec<DeliveryRecord>, AppError>
where
    F: FnMut() -> Result<String, AppError>,
{
    envelope.validate_bound(tenant, event, None)?;
    let mut statement = tx
        .prepare(
            "SELECT id FROM org_webhooks \
         WHERE org = ?1 \
           AND instr(',' || events || ',', ',' || ?2 || ',') > 0 \
           AND (?3 IS NULL OR id <> ?3) \
         ORDER BY created_at ASC, id ASC",
        )
        .map_err(internal)?;
    let ids = statement
        .query_map(
            params![envelope.tenant(), event_name(event), policy.excluded_target],
            |row| row.get::<_, String>(0),
        )
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    let payload = envelope.canonical_bytes()?;
    let digest = envelope.payload_sha256()?;
    let inputs = ids
        .into_iter()
        .map(|target_key| {
            Ok((
                EnqueueDelivery {
                    event_id: envelope.event_id().into(),
                    tenant: envelope.tenant().into(),
                    event_type: envelope.event_type().into(),
                    target_key: target_key.clone(),
                    secret_ref: format!("webhook:{target_key}"),
                    payload: payload.clone(),
                    payload_sha256: Some(digest.clone()),
                    durability_intent_id: policy.durability_intent_id.clone(),
                    delivery_kind: outbox::DELIVERY_KIND_EVENT.to_owned(),
                    ordering_key: target_key,
                    depends_on_outbox_id: None,
                },
                next_id()?,
            ))
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    outbox::enqueue_many_in_transaction(tx, &inputs, now)
}
fn internal(_: rusqlite::Error) -> AppError {
    AppError::Internal
}
