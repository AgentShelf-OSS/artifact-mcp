//! Notification fan-out planning for feedback events, with stable event IDs.

use rusqlite::{Transaction, params};

use crate::{
    error::AppError,
    integrations::delivery_envelope::{DeliveryEnvelopeV1, stable_delivery_event_id},
    model::{ArtifactMeta, Feedback, FeedbackId, NotificationPayload, OrgId, WebhookEvent},
    persistence::{discussions::active_plan_in_transaction, outbox_fanout},
};

use super::{DeliveryPlanningContext, database_failure};

pub(super) fn plan_event_at(
    tx: &Transaction<'_>,
    planning: &DeliveryPlanningContext,
    public_base_url: &str,
    artifact: &ArtifactMeta,
    feedback: &Feedback,
    event: WebhookEvent,
    now: i64,
) -> Result<(), AppError> {
    let (viewer_email, body, resolver) = match event {
        WebhookEvent::Feedback => (
            feedback.viewer_email.clone(),
            Some(feedback.body.clone()),
            None,
        ),
        WebhookEvent::Resolved => (None, None, feedback.resolved_by.clone()),
        _ => {
            return Err(AppError::Validation(
                "feedback delivery event is unsupported".to_owned(),
            ));
        }
    };
    let payload = NotificationPayload {
        artifact_id: artifact.id.clone(),
        title: artifact.title.clone(),
        url: format!("{public_base_url}/{}", artifact.id),
        description: artifact.description.clone(),
        uploader_label: artifact.uploader_label.clone(),
        category: artifact.category.clone(),
        revision: artifact.revision,
        bytes: artifact.bytes,
        viewer_email,
        body,
        resolver,
    };
    let event_id = delivery_event_id(tx, &artifact.org, &event, &feedback.id)?;
    let envelope = DeliveryEnvelopeV1::build(event_id, &artifact.org, &event, &payload)?;
    let excluded_target = active_plan_in_transaction(tx, &artifact.id, &artifact.org)?
        .and_then(|plan| plan.notification_webhook_id);
    outbox_fanout::fanout_in_transaction_excluding(
        tx,
        &envelope,
        &artifact.org,
        &event,
        now,
        excluded_target.as_deref(),
        || Ok(planning.next_id()),
    )?;
    Ok(())
}

fn delivery_event_id(
    tx: &Transaction<'_>,
    org: &OrgId,
    event: &WebhookEvent,
    feedback_id: &FeedbackId,
) -> Result<String, AppError> {
    if event == &WebhookEvent::Feedback {
        let subject = format!("feedback:{}", feedback_id.0);
        return Ok(stable_delivery_event_id(org, event, &subject));
    }
    for resolution in 1_u64.. {
        let subject = format!("feedback:{}:resolution:{resolution}", feedback_id.0);
        let candidate = stable_delivery_event_id(org, event, &subject);
        let exists = tx
            .query_row(
                "SELECT EXISTS(\
                   SELECT 1 FROM provider_delivery_outbox \
                   WHERE provider = 'discord' AND tenant = ?1 AND event_type = 'resolved' \
                     AND event_id = ?2\
                 )",
                params![org.0, candidate],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| database_failure("select feedback delivery event", &error))?
            != 0;
        if !exists {
            return Ok(candidate);
        }
    }
    Err(AppError::Internal)
}
