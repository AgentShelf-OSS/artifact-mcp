//! Pre-send discussion stages. None of them performs provider I/O.

use super::{DiscussionBinding, DiscussionExit, DiscussionKind, DiscussionTargets};
use crate::{
    integrations::{
        delivery_worker::{
            DiscussionResolutionFailure, WorkerDiscussions, backoff::backoff,
            outcome::terminal_code,
        },
        discord_discussion::{DiscussionOperation, DiscussionRequest, discussion_request},
        discussion_envelope::{DiscordDiscussionEnvelopeV1, DiscordDiscussionOperationV1},
    },
    model::{ArtifactId, OrgId},
    persistence::{
        discussions::{
            ArtifactDiscussion, DiscussionConnectionDelivery, DiscussionConnectionStrategy,
            DiscussionMessageLink, discussion_ordering_key,
        },
        outbox::{
            DELIVERY_KIND_DISCUSSION_MESSAGE, DELIVERY_KIND_DISCUSSION_THREAD,
            DELIVERY_KIND_DISCUSSION_TOMBSTONE, DeliveryRecord, verify_payload_hash,
        },
    },
};

/// Stage 1: verify the payload hash and strictly decode the canonical envelope. Failures here
/// cannot yet be bound to operation state, so they are plain outbox dead-letter classes.
pub(super) fn decode_discussion(
    record: &DeliveryRecord,
) -> Result<(DiscordDiscussionEnvelopeV1, DiscussionBinding<'_>), &'static str> {
    if !verify_payload_hash(record) {
        return Err("payload_hash_mismatch");
    }
    let tenant = OrgId(record.tenant.clone());
    // The artifact/connection/generation values are first recovered from the canonical
    // envelope. Only then can failures update operation-specific state.
    let parsed: DiscordDiscussionEnvelopeV1 =
        serde_json::from_slice(&record.payload).map_err(|_| "validation_failed")?;
    let artifact = ArtifactId(parsed.artifact_id().to_owned());
    let connection = parsed.connection_id().to_owned();
    let generation = parsed.generation();
    let envelope = DiscordDiscussionEnvelopeV1::decode_canonical(
        &record.payload,
        &tenant,
        &record.event_id,
        parsed.artifact_id(),
        &connection,
        generation,
        Some(&record.payload_sha256),
    )
    .map_err(|_| "validation_failed")?;
    let feedback = crate::model::FeedbackId(envelope.operation().feedback_id().to_owned());
    Ok((
        envelope,
        DiscussionBinding {
            record,
            tenant,
            artifact,
            connection,
            generation,
            feedback,
        },
    ))
}

/// Stages 2-6: everything between a trusted envelope and an executable provider request. No
/// stage performs provider I/O.
pub(super) async fn prepare_discussion(
    discussions: &dyn WorkerDiscussions,
    envelope: &DiscordDiscussionEnvelopeV1,
    kind: DiscussionKind,
    binding: &DiscussionBinding<'_>,
) -> Result<(DiscussionRequest, DiscussionTargets), DiscussionExit> {
    authorize_claim(envelope, binding)?;
    let discussion = authorize_generation(discussions, binding).await?;
    let link = verified_link(discussions, envelope, binding).await?;
    let targets = resolve_targets(kind, &discussion, &link)?;
    let delivery = resolve_connection(discussions, binding).await?;
    let operation =
        build_operation(discussions, envelope, kind, binding, &delivery, &targets).await?;
    let request = discussion_request(&delivery.url, binding.record.secret_ref.clone(), operation)
        .map_err(|reason| DiscussionExit::Terminal(terminal_code(reason)))?;
    Ok((request, targets))
}

/// Stage 2: the claimed row's kind, target, secret reference, and ordering key must all match
/// the envelope.
fn authorize_claim(
    envelope: &DiscordDiscussionEnvelopeV1,
    binding: &DiscussionBinding<'_>,
) -> Result<(), DiscussionExit> {
    let record = binding.record;
    if !discussion_kind_matches(&record.delivery_kind, envelope.operation())
        || record.target_key != binding.connection
        || record.secret_ref != format!("discussion:{}", binding.connection)
        || record.ordering_key != discussion_ordering_key(&binding.artifact, binding.generation)?
    {
        return Err(DiscussionExit::INVALID);
    }
    Ok(())
}

/// Stage 3: the artifact's current discussion must still be bound to this connection and
/// generation.
async fn authorize_generation(
    discussions: &dyn WorkerDiscussions,
    binding: &DiscussionBinding<'_>,
) -> Result<ArtifactDiscussion, DiscussionExit> {
    let Some(discussion) = discussions
        .discussion(&binding.artifact, &binding.tenant)
        .await?
    else {
        return Err(DiscussionExit::INVALID);
    };
    // Paused is intentionally accepted here: disabling only stops new producer work. Any
    // re-enable increments generation and is rejected by this exact authority check.
    if discussion.connection_id.as_deref() != Some(binding.connection.as_str())
        || discussion.generation != binding.generation
    {
        return Err(DiscussionExit::INVALID);
    }
    Ok(discussion)
}

/// Stage 3b: the feedback mapping must exist and correlate with this exact row.
async fn verified_link(
    discussions: &dyn WorkerDiscussions,
    envelope: &DiscordDiscussionEnvelopeV1,
    binding: &DiscussionBinding<'_>,
) -> Result<DiscussionMessageLink, DiscussionExit> {
    let link = discussions
        .feedback_link(&binding.artifact, &binding.tenant, &binding.feedback)
        .await?;
    match link {
        Some(link) if valid_discussion_link(binding.record, envelope, Some(&link)) => Ok(link),
        _ => Err(DiscussionExit::INVALID),
    }
}

/// Stage 4: resolve the external thread/message ids each operation kind needs.
fn resolve_targets(
    kind: DiscussionKind,
    discussion: &ArtifactDiscussion,
    link: &DiscussionMessageLink,
) -> Result<DiscussionTargets, DiscussionExit> {
    match kind {
        DiscussionKind::Thread => Ok(DiscussionTargets {
            thread_id: None,
            message_id: None,
        }),
        DiscussionKind::Reply | DiscussionKind::Marker => {
            let thread = discussion
                .thread_id
                .clone()
                .ok_or(DiscussionExit::INVALID)?;
            Ok(DiscussionTargets {
                thread_id: Some(thread),
                message_id: None,
            })
        }
        DiscussionKind::Tombstone => {
            let (Some(thread), Some(message)) = (
                link.external_thread_id.clone(),
                link.external_message_id.clone(),
            ) else {
                return Err(DiscussionExit::INVALID);
            };
            Ok(DiscussionTargets {
                thread_id: Some(thread),
                message_id: Some(message),
            })
        }
    }
}

/// Stage 5: resolve the tenant-bound connection credential. Only an operational lookup failure
/// is retryable; it happens before provider I/O, so it carries no duplicate risk.
async fn resolve_connection(
    discussions: &dyn WorkerDiscussions,
    binding: &DiscussionBinding<'_>,
) -> Result<DiscussionConnectionDelivery, DiscussionExit> {
    match discussions
        .connection(&binding.connection, &binding.tenant)
        .await
    {
        Ok(Some(value)) if value.org == binding.tenant => Ok(value),
        Ok(Some(_) | None) | Err(DiscussionResolutionFailure::InvalidReference) => {
            Err(DiscussionExit::INVALID)
        }
        Err(DiscussionResolutionFailure::Retryable) => Err(DiscussionExit::Retry {
            class: "network",
            duplicate_risk: false,
            delay: backoff(binding.record.attempts),
        }),
        Err(DiscussionResolutionFailure::DecryptFailed) => {
            Err(DiscussionExit::Terminal("decrypt_failed"))
        }
    }
}

/// Stage 6: build the transport operation. A notification-thread connection creates the first
/// thread from the already delivered anchor message instead of a forum post.
async fn build_operation(
    discussions: &dyn WorkerDiscussions,
    envelope: &DiscordDiscussionEnvelopeV1,
    kind: DiscussionKind,
    binding: &DiscussionBinding<'_>,
    delivery: &DiscussionConnectionDelivery,
    targets: &DiscussionTargets,
) -> Result<DiscussionOperation, DiscussionExit> {
    let base_operation = envelope
        .to_transport_operation(targets.thread_id.as_deref(), targets.message_id.as_deref())
        .map_err(|_| DiscussionExit::INVALID)?;
    if kind != DiscussionKind::Thread
        || delivery.strategy != DiscussionConnectionStrategy::NotificationThread
    {
        return Ok(base_operation);
    }
    let anchor_message_id = discussions
        .notification_anchor(
            binding
                .record
                .depends_on_outbox_id
                .as_deref()
                .unwrap_or_default(),
            &binding.artifact,
            &binding.tenant,
            &binding.connection,
            binding.generation,
        )
        .await?
        .ok_or(DiscussionExit::INVALID)?;
    let channel_id = delivery.channel_id.clone().ok_or(DiscussionExit::INVALID)?;
    let DiscussionOperation::CreateThread {
        thread_name,
        content,
    } = base_operation
    else {
        return Err(DiscussionExit::INVALID);
    };
    DiscussionOperation::create_thread_from_message(
        channel_id,
        anchor_message_id,
        thread_name,
        content,
    )
    .map_err(|reason| DiscussionExit::Terminal(terminal_code(reason)))
}

fn discussion_kind_matches(kind: &str, operation: &DiscordDiscussionOperationV1) -> bool {
    matches!(
        (kind, operation),
        (
            DELIVERY_KIND_DISCUSSION_THREAD,
            DiscordDiscussionOperationV1::Thread { .. }
        ) | (
            DELIVERY_KIND_DISCUSSION_MESSAGE,
            DiscordDiscussionOperationV1::Reply { .. }
        ) | (
            DELIVERY_KIND_DISCUSSION_MESSAGE,
            DiscordDiscussionOperationV1::Resolved { .. }
        ) | (
            DELIVERY_KIND_DISCUSSION_MESSAGE,
            DiscordDiscussionOperationV1::Reopened { .. }
        ) | (
            DELIVERY_KIND_DISCUSSION_TOMBSTONE,
            DiscordDiscussionOperationV1::Tombstone { .. }
        )
    )
}

fn valid_discussion_link(
    record: &DeliveryRecord,
    envelope: &DiscordDiscussionEnvelopeV1,
    link: Option<&DiscussionMessageLink>,
) -> bool {
    let Some(link) = link else {
        return false;
    };
    if link.artifact_id.0 != envelope.artifact_id()
        || link.org.0 != envelope.tenant()
        || link.connection_id != envelope.connection_id()
        || link.generation != envelope.generation()
        || link.feedback_id.0 != envelope.operation().feedback_id()
    {
        return false;
    }
    match envelope.operation() {
        DiscordDiscussionOperationV1::Thread { .. }
        | DiscordDiscussionOperationV1::Reply { .. } => link.outbox_id == record.id,
        DiscordDiscussionOperationV1::Tombstone { .. } => {
            link.tombstone_outbox_id.as_deref() == Some(&record.id)
        }
        // A marker is correlated by its canonical receipt, but must refer to a retained original
        // feedback mapping in this exact generation.
        DiscordDiscussionOperationV1::Resolved { .. }
        | DiscordDiscussionOperationV1::Reopened { .. } => true,
    }
}
