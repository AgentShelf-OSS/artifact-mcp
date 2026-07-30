//! Atomic feedback mutations and durable provider-delivery planning.
//!
//! Route authorization remains the first request boundary, but mutable artifact and feedback
//! scope is reloaded after an immediate SQLite transaction begins. This closes the gap where an
//! artifact can move tenants between route authorization and the feedback mutation.

use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::{
    config::IdSource,
    error::AppError,
    integrations::{
        delivery_envelope::{DeliveryEnvelopeV1, stable_delivery_event_id},
        discord_discussion::MAX_DISCUSSION_CONTENT_CHARS,
        discussion_envelope::{DiscordDiscussionEnvelopeV1, DiscordDiscussionOperationV1},
    },
    model::{
        ArtifactId, ArtifactMeta, ClientId, Feedback, FeedbackId, FeedbackMutation, FeedbackRef,
        NotificationPayload, OrgId, SubmitFeedback, Timestamp, Viewer, WebhookEvent,
    },
    persistence::{
        discussions::{
            ActiveDiscussionPlan, CreateDiscussionMessageLink, DiscussionMessageLink,
            active_plan_in_transaction, create_link_in_transaction,
            latest_link_for_feedback_in_transaction, link_for_feedback_generation_in_transaction,
            mark_deleted_and_bind_tombstone_in_transaction, retained_tombstone_plan_in_transaction,
            root_outbox_in_transaction,
        },
        feedback::{self, NewFeedback},
        outbox::{
            DELIVERY_KIND_DISCUSSION_MESSAGE, DELIVERY_KIND_DISCUSSION_THREAD,
            DELIVERY_KIND_DISCUSSION_TOMBSTONE, EnqueueDelivery, OutboxClock, OutboxIdGenerator,
            RandomOutboxId, SystemOutboxClock, enqueue_in_transaction,
        },
        outbox_fanout,
    },
};

/// Injectable, non-I/O inputs used while planning provider-delivery rows.
#[derive(Clone)]
pub struct DeliveryPlanningContext {
    clock: Arc<dyn OutboxClock>,
    ids: Arc<dyn OutboxIdGenerator>,
}

impl DeliveryPlanningContext {
    /// Production clock and opaque outbox identifiers.
    #[must_use]
    pub fn production() -> Self {
        Self::new(Arc::new(SystemOutboxClock), Arc::new(RandomOutboxId))
    }

    /// Deterministic seam for service tests.
    #[must_use]
    pub const fn new(clock: Arc<dyn OutboxClock>, ids: Arc<dyn OutboxIdGenerator>) -> Self {
        Self { clock, ids }
    }

    fn now_millis(&self) -> i64 {
        self.clock.now_millis()
    }

    fn next_id(&self) -> String {
        self.ids.next_id()
    }
}

/// Inserts feedback and subscriber outbox rows in one transaction.
pub fn submit(
    conn: &mut Connection,
    feedback_ids: &dyn IdSource,
    planning: &DeliveryPlanningContext,
    public_base_url: &str,
    authorized: &ArtifactMeta,
    submission: &SubmitFeedback,
    max_body: u64,
) -> Result<Feedback, AppError> {
    let now = planning.now_millis();
    let tx = begin(conn)?;
    let artifact = revalidate_artifact(&tx, authorized)?;
    let created = feedback::add(
        &tx,
        feedback_ids,
        &NewFeedback {
            artifact_id: &artifact.id,
            org: &artifact.org,
            artifact_revision: artifact.revision,
            anchor_page: submission.anchor_page.as_deref(),
            submission,
            max_body,
        },
    )?;
    plan_event_at(
        &tx,
        planning,
        public_base_url,
        &artifact,
        &created,
        WebhookEvent::Feedback,
        now,
    )?;
    plan_discussion_feedback_at(&tx, planning, public_base_url, &artifact, &created, now)?;
    commit(tx)?;
    Ok(created)
}

/// Deletes viewer-owned feedback in an explicit transaction. Delete is intentionally local-only.
pub fn delete_as_viewer(
    conn: &mut Connection,
    authorized: &ArtifactMeta,
    viewer: &Viewer,
    id: FeedbackId,
) -> Result<FeedbackMutation, AppError> {
    let tx = begin(conn)?;
    let artifact = revalidate_artifact(&tx, authorized)?;
    let scope = FeedbackRef {
        id,
        artifact_id: artifact.id,
        org: artifact.org,
    };
    let mutation = feedback::delete_as_viewer(
        &tx,
        &scope,
        &viewer.email.clone().unwrap_or_default(),
        viewer.is_admin,
    )?;
    commit(tx)?;
    Ok(mutation)
}

/// Deletes feedback and, only while an active mirror exists, appends the correlated tombstone
/// work in the same transaction. Production passes its shared deterministic planning context;
/// the legacy wrapper above retains its historical local-only behavior.
pub fn delete_as_viewer_with_delivery(
    conn: &mut Connection,
    planning: &DeliveryPlanningContext,
    authorized: &ArtifactMeta,
    viewer: &Viewer,
    id: FeedbackId,
) -> Result<FeedbackMutation, AppError> {
    let now = planning.now_millis();
    let tx = begin(conn)?;
    let artifact = revalidate_artifact(&tx, authorized)?;
    let scope = FeedbackRef {
        id,
        artifact_id: artifact.id.clone(),
        org: artifact.org.clone(),
    };
    let existing_link =
        latest_link_for_feedback_in_transaction(&tx, &artifact.id, &artifact.org, &scope.id)?;
    let tombstone_plan = existing_link
        .as_ref()
        .map(|link| retained_tombstone_plan_in_transaction(&tx, link))
        .transpose()?
        .flatten();
    let mutation = feedback::delete_as_viewer(
        &tx,
        &scope,
        &viewer.email.clone().unwrap_or_default(),
        viewer.is_admin,
    )?;
    if mutation.changed
        && let (Some(plan), Some(link)) = (tombstone_plan.as_ref(), existing_link.as_ref())
    {
        plan_discussion_tombstone_at(&tx, planning, &artifact, plan, link, now)?;
    }
    commit(tx)?;
    Ok(mutation)
}

/// Resolves viewer-owned feedback and plans one durable event only for the state transition.
pub fn resolve_as_viewer(
    conn: &mut Connection,
    planning: &DeliveryPlanningContext,
    public_base_url: &str,
    authorized: &ArtifactMeta,
    viewer: &Viewer,
    id: FeedbackId,
) -> Result<FeedbackMutation, AppError> {
    let now = planning.now_millis();
    let tx = begin(conn)?;
    let artifact = revalidate_artifact(&tx, authorized)?;
    let scope = FeedbackRef {
        id,
        artifact_id: artifact.id.clone(),
        org: artifact.org.clone(),
    };
    let mutation = feedback::resolve_as_viewer(
        &tx,
        &scope,
        &viewer.email.clone().unwrap_or_default(),
        viewer.is_admin,
    )?;
    if mutation.changed {
        let persisted = scoped_feedback(&tx, &artifact, &mutation.id)?;
        if matches!(
            persisted.author,
            crate::model::FeedbackAuthor::Artifact { .. }
        ) {
            plan_event_at(
                &tx,
                planning,
                public_base_url,
                &artifact,
                &persisted,
                WebhookEvent::Resolved,
                now,
            )?;
            plan_discussion_marker_at(
                &tx,
                planning,
                &artifact,
                &persisted,
                DiscussionMarker::Resolved,
                now,
            )?;
        }
    }
    commit(tx)?;
    Ok(mutation)
}

/// Resolves publisher-owned feedback and plans one durable event only for the state transition.
pub fn resolve_as_publisher(
    conn: &mut Connection,
    planning: &DeliveryPlanningContext,
    public_base_url: &str,
    authorized: &ArtifactMeta,
    id: FeedbackId,
    resolved_by: &str,
) -> Result<bool, AppError> {
    let now = planning.now_millis();
    let tx = begin(conn)?;
    let artifact = revalidate_artifact(&tx, authorized)?;
    let scoped = scoped_feedback(&tx, &artifact, &id)?;
    let changed = feedback::resolve_as_publisher(&tx, &scoped.id, resolved_by)?;
    if changed && matches!(scoped.author, crate::model::FeedbackAuthor::Artifact { .. }) {
        let persisted = scoped_feedback(&tx, &artifact, &scoped.id)?;
        plan_event_at(
            &tx,
            planning,
            public_base_url,
            &artifact,
            &persisted,
            WebhookEvent::Resolved,
            now,
        )?;
        plan_discussion_marker_at(
            &tx,
            planning,
            &artifact,
            &persisted,
            DiscussionMarker::Resolved,
            now,
        )?;
    }
    commit(tx)?;
    Ok(changed)
}

/// Reopens publisher-owned feedback in an explicit transaction. Reopen is local-only for now.
pub fn reopen_as_publisher(
    conn: &mut Connection,
    authorized: &ArtifactMeta,
    id: FeedbackId,
) -> Result<bool, AppError> {
    let tx = begin(conn)?;
    let artifact = revalidate_artifact(&tx, authorized)?;
    let scoped = scoped_feedback(&tx, &artifact, &id)?;
    let changed = feedback::reopen(&tx, &scoped.id)?;
    commit(tx)?;
    Ok(changed)
}

/// Reopening is local-only unless the feedback has a current-generation mirrored message.  The
/// marker has no message link of its own, keeping external message evidence one-to-one with user
/// feedback.
pub fn reopen_as_publisher_with_delivery(
    conn: &mut Connection,
    planning: &DeliveryPlanningContext,
    authorized: &ArtifactMeta,
    id: FeedbackId,
) -> Result<bool, AppError> {
    let now = planning.now_millis();
    let tx = begin(conn)?;
    let artifact = revalidate_artifact(&tx, authorized)?;
    let scoped = scoped_feedback(&tx, &artifact, &id)?;
    let changed = feedback::reopen(&tx, &scoped.id)?;
    if changed && matches!(scoped.author, crate::model::FeedbackAuthor::Artifact { .. }) {
        let persisted = scoped_feedback(&tx, &artifact, &scoped.id)?;
        plan_discussion_marker_at(
            &tx,
            planning,
            &artifact,
            &persisted,
            DiscussionMarker::Reopened,
            now,
        )?;
    }
    commit(tx)?;
    Ok(changed)
}

fn begin(conn: &mut Connection) -> Result<Transaction<'_>, AppError> {
    conn.transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| database_failure("begin feedback transaction", &error))
}

fn commit(tx: Transaction<'_>) -> Result<(), AppError> {
    tx.commit()
        .map_err(|error| database_failure("commit feedback transaction", &error))
}

fn revalidate_artifact(
    tx: &Transaction<'_>,
    authorized: &ArtifactMeta,
) -> Result<ArtifactMeta, AppError> {
    let persisted = tx
        .query_row(
            "SELECT a.id, a.client_id, a.org, a.title, a.description, a.bytes, a.created_at, \
                    a.updated_at, a.uploader_label, a.owner_email, a.is_bundle, a.entry, \
                    a.revision, a.category, a.hidden, a.body_sha256 \
             FROM artifacts a \
             WHERE a.id = ?1 \
               AND NOT EXISTS (\
                 SELECT 1 FROM artifact_durability_intents i WHERE i.artifact_id = a.id\
               )",
            params![authorized.id.0],
            artifact_from_row,
        )
        .optional()
        .map_err(|error| database_failure("reload feedback artifact", &error))?;
    persisted
        .filter(|persisted| same_authorization_scope(persisted, authorized))
        .ok_or_else(not_found)
}

fn same_authorization_scope(persisted: &ArtifactMeta, authorized: &ArtifactMeta) -> bool {
    persisted.id == authorized.id
        && persisted.org == authorized.org
        && persisted.client_id == authorized.client_id
        && persisted.owner_email == authorized.owner_email
}

fn scoped_feedback(
    tx: &Transaction<'_>,
    artifact: &ArtifactMeta,
    id: &FeedbackId,
) -> Result<Feedback, AppError> {
    feedback::get(tx, id)?
        .filter(|row| row.artifact_id == artifact.id && row.org == artifact.org)
        .ok_or_else(not_found)
}

fn plan_event_at(
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

#[derive(Clone, Copy)]
enum DiscussionMarker {
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
fn plan_discussion_feedback_at(
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
fn plan_discussion_marker_at(
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

fn plan_discussion_tombstone_at(
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

fn artifact_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactMeta> {
    Ok(ArtifactMeta {
        id: ArtifactId(row.get(0)?),
        client_id: ClientId(row.get(1)?),
        org: OrgId(row.get(2)?),
        title: row.get(3)?,
        description: row.get(4)?,
        bytes: nonnegative_u64(row, 5)?,
        created_at: Timestamp(row.get(6)?),
        updated_at: Timestamp(row.get(7)?),
        uploader_label: row.get(8)?,
        owner_email: row.get(9)?,
        is_bundle: row.get::<_, i64>(10)? != 0,
        entry: row.get(11)?,
        revision: nonnegative_u64(row, 12)?,
        category: row.get(13)?,
        hidden: row.get::<_, i64>(14)? != 0,
        body_sha256: row.get(15)?,
    })
}

fn nonnegative_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn not_found() -> AppError {
    AppError::NotFound(feedback::NOT_FOUND_MESSAGE.to_owned())
}

fn database_failure(operation: &str, error: &rusqlite::Error) -> AppError {
    tracing::error!(operation, error = %error, "feedback transaction failed");
    AppError::Unavailable("database unavailable".to_owned())
}
