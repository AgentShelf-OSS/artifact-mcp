//! Atomic feedback mutations and durable provider-delivery planning.
//!
//! Route authorization remains the first request boundary, but mutable artifact and feedback
//! scope is reloaded after an immediate SQLite transaction begins. This closes the gap where an
//! artifact can move tenants between route authorization and the feedback mutation.

mod discussion;
mod notifications;

use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::{
    config::IdSource,
    error::AppError,
    model::{
        ArtifactMeta, Feedback, FeedbackId, FeedbackMutation, FeedbackRef, SubmitFeedback, Viewer,
        WebhookEvent,
    },
    persistence::{
        artifact_rows::{META_COLUMNS, meta_from_row},
        discussions::{
            latest_link_for_feedback_in_transaction, retained_tombstone_plan_in_transaction,
        },
        feedback::{self, NewFeedback},
        outbox::{OutboxClock, OutboxIdGenerator, RandomOutboxId, SystemOutboxClock},
    },
};

use discussion::{
    DiscussionMarker, plan_discussion_feedback_at, plan_discussion_marker_at,
    plan_discussion_tombstone_at,
};
use notifications::plan_event_at;

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
            &format!(
                "SELECT {META_COLUMNS} FROM artifacts a \
                 WHERE a.id = ?1 \
                   AND NOT EXISTS (\
                     SELECT 1 FROM artifact_durability_intents i WHERE i.artifact_id = a.id\
                   )"
            ),
            params![authorized.id.0],
            meta_from_row,
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

fn not_found() -> AppError {
    AppError::NotFound(feedback::NOT_FOUND_MESSAGE.to_owned())
}

fn database_failure(operation: &str, error: &rusqlite::Error) -> AppError {
    tracing::error!(operation, error = %error, "feedback transaction failed");
    AppError::Unavailable("database unavailable".to_owned())
}
