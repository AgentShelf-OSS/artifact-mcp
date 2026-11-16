//! Discussion delivery planning: feedback messages, markers and tombstones, with thread names and event IDs.

use rusqlite::{Transaction, params};

use crate::{
    error::AppError,
    integrations::{
        delivery_envelope::stable_delivery_event_id,
        discord_discussion::MAX_DISCUSSION_CONTENT_CHARS,
        discussion_envelope::{DiscordDiscussionEnvelopeV1, DiscordDiscussionOperationV1},
    },
    model::{ArtifactMeta, Feedback, OrgId, WebhookEvent},
    persistence::{
        discussions::{
            ActiveDiscussionPlan, CreateDiscussionMessageLink, DiscussionMessageLink,
            active_plan_in_transaction, create_link_in_transaction,
            link_for_feedback_generation_in_transaction,
            mark_deleted_and_bind_tombstone_in_transaction, root_outbox_in_transaction,
        },
        outbox::{
            DELIVERY_KIND_DISCUSSION_MESSAGE, DELIVERY_KIND_DISCUSSION_THREAD,
            DELIVERY_KIND_DISCUSSION_TOMBSTONE, EnqueueDelivery, enqueue_in_transaction,
        },
    },
};

use super::{DeliveryPlanningContext, database_failure};

#[derive(Clone, Copy)]
pub(super) enum DiscussionMarker {
    Resolved,
    Reopened,
}

impl DiscussionMarker {
    const fn event_type(self) -> &'static str {
        match self {
            Self::Resolved => "discussion_resolved",
            Self::Reopened => "discussion_reopened",
        }
    }

    fn operation(self, feedback_id: String) -> Result<DiscordDiscussionOperationV1, AppError> {
        match self {
            Self::Resolved => DiscordDiscussionOperationV1::resolved(feedback_id),
            Self::Reopened => DiscordDiscussionOperationV1::reopened(feedback_id),
        }
    }
}

/// Add a root on the first current-generation feedback, otherwise a direct-root reply.  The
/// feedback row, outbox row, and retained correlation are one SQLite transaction, so an outbox
/// capacity rejection cannot leave a canonical comment with a phantom mirror mapping.
pub(super) fn plan_discussion_feedback_at(
    tx: &Transaction<'_>,
    planning: &DeliveryPlanningContext,
    public_base_url: &str,
    artifact: &ArtifactMeta,
    feedback: &Feedback,
    now: i64,
) -> Result<(), AppError> {
    let Some(plan) = active_plan_in_transaction(tx, &artifact.id, &artifact.org)? else {
        return Ok(());
    };
    if link_for_feedback_generation_in_transaction(
        tx,
        &artifact.id,
        &artifact.org,
        &feedback.id,
        plan.generation,
    )?
    .is_some()
    {
        return Ok(());
    }
    let root = root_outbox_in_transaction(tx, &artifact.id, &artifact.org, &plan)?;
    let (kind, operation, subject, dependency) = if let Some(root) = root {
        (
            DELIVERY_KIND_DISCUSSION_MESSAGE,
            DiscordDiscussionOperationV1::reply(
                feedback.id.0.clone(),
                discussion_content(public_base_url, artifact, feedback),
            )?,
            format!(
                "discussion:{}:{}:feedback:{}:reply",
                artifact.id.0, plan.generation, feedback.id.0
            ),
            Some(root),
        )
    } else {
        (
            DELIVERY_KIND_DISCUSSION_THREAD,
            DiscordDiscussionOperationV1::thread(
                feedback.id.0.clone(),
                discussion_thread_name(&artifact.title),
                discussion_content(public_base_url, artifact, feedback),
            )?,
            format!(
                "discussion:{}:{}:feedback:{}:thread",
                artifact.id.0, plan.generation, feedback.id.0
            ),
            plan.anchor_outbox_id.clone(),
        )
    };
    let event_id =
        next_discussion_event_id(tx, &artifact.org, &plan.connection_id, kind, &subject)?;
    let envelope = DiscordDiscussionEnvelopeV1::build(
        event_id.clone(),
        &artifact.org,
        artifact.id.0.clone(),
        plan.connection_id.clone(),
        plan.generation,
        operation,
    )?;
    let payload = envelope.canonical_bytes()?;
    let record = enqueue_in_transaction(
        tx,
        &discussion_delivery_input(
            event_id.clone(),
            &artifact.org,
            &plan,
            kind,
            kind,
            payload,
            dependency,
        ),
        &planning.next_id(),
        now,
    )?;
    create_link_in_transaction(
        tx,
        &CreateDiscussionMessageLink {
            artifact_id: artifact.id.clone(),
            org: artifact.org.clone(),
            connection_id: plan.connection_id.clone(),
            feedback_id: feedback.id.clone(),
            delivery_event_id: event_id,
            outbox_id: record.id,
            external_thread_id: None,
            generation: plan.generation,
        },
    )?;
    Ok(())
}

/// Mirror resolution transitions only after a user-authored feedback link exists in this active
/// generation.  Marker rows deliberately have no `discussion_message_links` entry.
pub(super) fn plan_discussion_marker_at(
    tx: &Transaction<'_>,
    planning: &DeliveryPlanningContext,
    artifact: &ArtifactMeta,
    feedback: &Feedback,
    marker: DiscussionMarker,
    now: i64,
) -> Result<(), AppError> {
    let Some(plan) = active_plan_in_transaction(tx, &artifact.id, &artifact.org)? else {
        return Ok(());
    };
    if link_for_feedback_generation_in_transaction(
        tx,
        &artifact.id,
        &artifact.org,
        &feedback.id,
        plan.generation,
    )?
    .is_none()
    {
        return Ok(());
    }
    let Some(root) = root_outbox_in_transaction(tx, &artifact.id, &artifact.org, &plan)? else {
        return Err(AppError::Internal);
    };
    let subject = format!(
        "discussion:{}:{}:feedback:{}:{}",
        artifact.id.0,
        plan.generation,
        feedback.id.0,
        marker.event_type()
    );
    let event_id = next_discussion_event_id(
        tx,
        &artifact.org,
        &plan.connection_id,
        marker.event_type(),
        &subject,
    )?;
    let envelope = DiscordDiscussionEnvelopeV1::build(
        event_id.clone(),
        &artifact.org,
        artifact.id.0.clone(),
        plan.connection_id.clone(),
        plan.generation,
        marker.operation(feedback.id.0.clone())?,
    )?;
    let payload = envelope.canonical_bytes()?;
    enqueue_in_transaction(
        tx,
        &discussion_delivery_input(
            event_id,
            &artifact.org,
            &plan,
            marker.event_type(),
            DELIVERY_KIND_DISCUSSION_MESSAGE,
            payload,
            Some(root),
        ),
        &planning.next_id(),
        now,
    )?;
    Ok(())
}

pub(super) fn plan_discussion_tombstone_at(
    tx: &Transaction<'_>,
    planning: &DeliveryPlanningContext,
    artifact: &ArtifactMeta,
    plan: &ActiveDiscussionPlan,
    link: &DiscussionMessageLink,
    now: i64,
) -> Result<(), AppError> {
    if link.tombstone_outbox_id.is_some() {
        return Ok(());
    }
    let Some(root) = root_outbox_in_transaction(tx, &artifact.id, &artifact.org, plan)? else {
        return Err(AppError::Internal);
    };
    let subject = format!(
        "discussion:{}:{}:feedback:{}:tombstone",
        artifact.id.0, plan.generation, link.feedback_id.0
    );
    let event_id = next_discussion_event_id(
        tx,
        &artifact.org,
        &plan.connection_id,
        DELIVERY_KIND_DISCUSSION_TOMBSTONE,
        &subject,
    )?;
    let envelope = DiscordDiscussionEnvelopeV1::build(
        event_id.clone(),
        &artifact.org,
        artifact.id.0.clone(),
        plan.connection_id.clone(),
        plan.generation,
        DiscordDiscussionOperationV1::tombstone(link.feedback_id.0.clone())?,
    )?;
    let payload = envelope.canonical_bytes()?;
    let record = enqueue_in_transaction(
        tx,
        &discussion_delivery_input(
            event_id,
            &artifact.org,
            plan,
            DELIVERY_KIND_DISCUSSION_TOMBSTONE,
            DELIVERY_KIND_DISCUSSION_TOMBSTONE,
            payload,
            Some(root),
        ),
        &planning.next_id(),
        now,
    )?;
    mark_deleted_and_bind_tombstone_in_transaction(tx, link, &record.id)
}

fn discussion_delivery_input(
    event_id: String,
    org: &OrgId,
    plan: &ActiveDiscussionPlan,
    event_type: &str,
    delivery_kind: &str,
    payload: Vec<u8>,
    depends_on_outbox_id: Option<String>,
) -> EnqueueDelivery {
    EnqueueDelivery {
        event_id,
        tenant: org.0.clone(),
        event_type: event_type.to_owned(),
        target_key: plan.connection_id.clone(),
        secret_ref: format!("discussion:{}", plan.connection_id),
        payload,
        payload_sha256: None,
        durability_intent_id: None,
        delivery_kind: delivery_kind.to_owned(),
        ordering_key: plan.ordering_key.clone(),
        depends_on_outbox_id,
    }
}

/// Stable intent IDs are scoped to a generation and never reuse a committed delivery row.  The
/// first candidate makes producer retries idempotent; a real resolve/reopen cycle receives the
/// next deterministic suffix after the prior transition has committed.
fn next_discussion_event_id(
    tx: &Transaction<'_>,
    org: &OrgId,
    connection_id: &str,
    event_type: &str,
    subject: &str,
) -> Result<String, AppError> {
    for sequence in 1_u64.. {
        let candidate_subject = if sequence == 1 {
            subject.to_owned()
        } else {
            format!("{subject}:{sequence}")
        };
        let candidate = stable_delivery_event_id(org, &WebhookEvent::Feedback, &candidate_subject);
        let exists: i64 = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM provider_delivery_outbox \
                   WHERE provider = 'discord' AND tenant = ?1 AND target_key = ?2 \
                     AND event_type = ?3 AND event_id = ?4)",
                params![org.0, connection_id, event_type, candidate],
                |row| row.get(0),
            )
            .map_err(|error| database_failure("select discussion delivery event", &error))?;
        if exists == 0 {
            return Ok(candidate);
        }
    }
    Err(AppError::Internal)
}

fn discussion_content(
    public_base_url: &str,
    artifact: &ArtifactMeta,
    feedback: &Feedback,
) -> String {
    let title: String = artifact.title.chars().take(160).collect();
    let author: String = match &feedback.author {
        crate::model::FeedbackAuthor::Artifact { viewer_email } => {
            viewer_email.0.chars().take(160).collect()
        }
        crate::model::FeedbackAuthor::Discord {
            external_author_display,
            ..
        } => {
            external_author_display
                .chars()
                .take(150)
                .collect::<String>()
                + " · Discord"
        }
    };
    let header = format!("Artifact: {title} · revision {}\n", artifact.revision);
    let footer = format!("\n\n{author}: ");
    let candidate_link = format!(
        "{}/{}",
        public_base_url.trim_end_matches('/'),
        artifact.id.0
    );
    // Preserve the authenticated deep link whenever it leaves room for at least one character of
    // the canonical feedback body. Pathological deployment URLs degrade to bounded status text
    // instead of making envelope construction roll back the local feedback transaction.
    let max_link_chars = MAX_DISCUSSION_CONTENT_CHARS
        .saturating_sub(header.chars().count())
        .saturating_sub(footer.chars().count())
        .saturating_sub(1);
    let link = if candidate_link.chars().count() <= max_link_chars {
        candidate_link
    } else {
        "Artifact link unavailable (configured base URL is too long)".to_owned()
    };
    let prefix = format!("{header}{link}{footer}");
    let available = MAX_DISCUSSION_CONTENT_CHARS.saturating_sub(prefix.chars().count());
    let content = format!(
        "{prefix}{}",
        feedback.body.chars().take(available).collect::<String>()
    );
    content.chars().take(MAX_DISCUSSION_CONTENT_CHARS).collect()
}

fn discussion_thread_name(title: &str) -> String {
    let trimmed = title.trim();
    let name: String = trimmed.chars().take(100).collect();
    if name.is_empty() {
        "Artifact discussion".to_owned()
    } else {
        name
    }
}
