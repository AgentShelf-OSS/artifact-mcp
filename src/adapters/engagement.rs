//! Reactions, views, feedback, and viewer notifications backed by the production SQLite pool.

use std::{collections::BTreeMap, sync::Arc};

use crate::{
    config::IdSource,
    error::AppError,
    model::{
        ArtifactId, EmailAddress, Feedback, FeedbackId, FeedbackMutation, FeedbackRef, OrgId,
        PublisherIdentity, Reaction, ReactionUpdate, Sentiment, SubmitFeedback, TopViewedArtifact,
        ViewCounts, Viewer, ViewerNotification, ViewerView,
    },
    persistence::{
        db::{self, DbPool},
        feedback, feedback_delivery,
        feedback_delivery::DeliveryPlanningContext,
        notifications, reactions, views,
    },
    ports::{BoxFuture, EngagementService},
};

#[derive(Clone)]
pub struct ProductionEngagement {
    pool: DbPool,
    ids: Arc<dyn IdSource>,
    feedback_max_body: u64,
    public_base_url: String,
    delivery_planning: DeliveryPlanningContext,
}

impl ProductionEngagement {
    pub fn new(
        pool: DbPool,
        ids: Arc<dyn IdSource>,
        feedback_max_body: u64,
        public_base_url: String,
        delivery_planning: DeliveryPlanningContext,
    ) -> Self {
        Self {
            pool,
            ids,
            feedback_max_body,
            public_base_url,
            delivery_planning,
        }
    }
}

impl EngagementService for ProductionEngagement {
    fn reaction<'a>(
        &'a self,
        artifact: &'a crate::security::access::AuthorizedArtifact,
        viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<Reaction, AppError>> {
        let email = viewer.email.clone().unwrap_or_default();
        let artifact_id = artifact.meta().id.clone();
        Box::pin(reactions::get_pooled(&self.pool, email, artifact_id))
    }

    fn set_reaction(
        &self,
        artifact: crate::security::access::AuthorizedArtifact,
        viewer: Viewer,
        update: ReactionUpdate,
    ) -> BoxFuture<'_, Result<Reaction, AppError>> {
        let email = viewer.email.unwrap_or_default();
        let artifact_id = artifact.meta().id.clone();
        Box::pin(reactions::set_pooled(
            &self.pool,
            email,
            artifact_id,
            update,
        ))
    }

    fn reactions_for_viewer<'a>(
        &'a self,
        viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<BTreeMap<ArtifactId, Reaction>, AppError>> {
        let email = viewer.email.clone().unwrap_or_default();
        Box::pin(reactions::for_viewer_pooled(&self.pool, email))
    }

    fn sentiment(&self) -> BoxFuture<'_, Result<BTreeMap<ArtifactId, Sentiment>, AppError>> {
        Box::pin(reactions::sentiment_pooled(&self.pool))
    }

    fn record_view<'a>(
        &'a self,
        artifact: &'a crate::security::access::AuthorizedArtifact,
        viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        let pool = self.pool.clone();
        let artifact_id = artifact.meta().id.clone();
        let org = artifact.meta().org.clone();
        let viewer = viewer.clone();
        Box::pin(async move {
            if !views::should_record(&viewer) {
                return Ok(());
            }
            let Some(email) = viewer.email else {
                return Ok(());
            };
            views::record_pooled(&pool, artifact_id, org, email).await
        })
    }

    fn view_counts<'a>(
        &'a self,
        artifact: &'a crate::security::access::AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<ViewCounts, AppError>> {
        Box::pin(views::counts_for_pooled(
            &self.pool,
            artifact.meta().id.clone(),
        ))
    }

    fn view_counts_for_org<'a>(
        &'a self,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<BTreeMap<ArtifactId, ViewCounts>, AppError>> {
        Box::pin(views::counts_for_org_pooled(&self.pool, org.clone()))
    }

    fn viewers<'a>(
        &'a self,
        artifact: &'a crate::security::access::AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Vec<ViewerView>, AppError>> {
        Box::pin(views::viewers_for_pooled(
            &self.pool,
            artifact.meta().id.clone(),
        ))
    }

    fn top_for_org<'a>(
        &'a self,
        org: &'a OrgId,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<TopViewedArtifact>, AppError>> {
        Box::pin(views::top_for_org_pooled(&self.pool, org.clone(), limit))
    }

    fn feedback_ref<'a>(
        &'a self,
        id: &'a FeedbackId,
    ) -> BoxFuture<'a, Result<Option<FeedbackRef>, AppError>> {
        let pool = self.pool.clone();
        let id = id.clone();
        Box::pin(
            async move { db::interact(&pool, move |conn| feedback::feedback_ref(conn, &id)).await },
        )
    }

    fn list_feedback<'a>(
        &'a self,
        artifact: &'a crate::security::access::AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Vec<Feedback>, AppError>> {
        let pool = self.pool.clone();
        let id = artifact.meta().id.clone();
        Box::pin(async move {
            db::interact(&pool, move |conn| feedback::list_for_artifact(conn, &id)).await
        })
    }

    fn submit_feedback(
        &self,
        artifact: crate::security::access::AuthorizedArtifact,
        submission: SubmitFeedback,
    ) -> BoxFuture<'_, Result<Feedback, AppError>> {
        let pool = self.pool.clone();
        let ids = Arc::clone(&self.ids);
        let max_body = self.feedback_max_body;
        let meta = artifact.into_meta();
        let public_base_url = self.public_base_url.clone();
        let planning = self.delivery_planning.clone();
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                feedback_delivery::submit(
                    conn,
                    ids.as_ref(),
                    &planning,
                    &public_base_url,
                    &meta,
                    &submission,
                    max_body,
                )
            })
            .await
        })
    }

    fn delete_feedback(
        &self,
        artifact: crate::security::access::AuthorizedArtifact,
        viewer: Viewer,
        id: FeedbackId,
    ) -> BoxFuture<'_, Result<FeedbackMutation, AppError>> {
        let pool = self.pool.clone();
        let meta = artifact.into_meta();
        let planning = self.delivery_planning.clone();
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                feedback_delivery::delete_as_viewer_with_delivery(
                    conn, &planning, &meta, &viewer, id,
                )
            })
            .await
        })
    }

    fn resolve_feedback_as_viewer(
        &self,
        artifact: crate::security::access::AuthorizedArtifact,
        viewer: Viewer,
        id: FeedbackId,
    ) -> BoxFuture<'_, Result<FeedbackMutation, AppError>> {
        let pool = self.pool.clone();
        let meta = artifact.into_meta();
        let public_base_url = self.public_base_url.clone();
        let planning = self.delivery_planning.clone();
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                feedback_delivery::resolve_as_viewer(
                    conn,
                    &planning,
                    &public_base_url,
                    &meta,
                    &viewer,
                    id,
                )
            })
            .await
        })
    }

    fn list_feedback_for_publisher<'a>(
        &'a self,
        publisher: &'a PublisherIdentity,
        artifact: Option<&'a crate::security::access::OwnedArtifact>,
    ) -> BoxFuture<'a, Result<Vec<Feedback>, AppError>> {
        let pool = self.pool.clone();
        let publisher = publisher.clone();
        let artifact_id = artifact.map(|owned| owned.meta().id.clone());
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                if publisher.is_admin() {
                    feedback::list_all(conn, artifact_id.as_ref())
                } else if matches!(publisher.role.as_str(), "reader" | "collaborator") {
                    feedback::list_for_org(conn, &publisher.org)
                } else {
                    feedback::list_for_client(
                        conn,
                        &publisher.client_id,
                        artifact_id.as_ref(),
                        Some(&publisher.org),
                    )
                }
            })
            .await
        })
    }

    fn resolve_feedback_as_publisher(
        &self,
        artifact: crate::security::access::OwnedArtifact,
        id: FeedbackId,
        resolved_by: String,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        let pool = self.pool.clone();
        let meta = artifact.meta().clone();
        let public_base_url = self.public_base_url.clone();
        let planning = self.delivery_planning.clone();
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                feedback_delivery::resolve_as_publisher(
                    conn,
                    &planning,
                    &public_base_url,
                    &meta,
                    id,
                    &resolved_by,
                )
            })
            .await
        })
    }

    fn reopen_feedback_as_publisher(
        &self,
        artifact: crate::security::access::OwnedArtifact,
        id: FeedbackId,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        let pool = self.pool.clone();
        let meta = artifact.meta().clone();
        let planning = self.delivery_planning.clone();
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                feedback_delivery::reopen_as_publisher_with_delivery(conn, &planning, &meta, id)
            })
            .await
        })
    }

    fn recent_notifications<'a>(
        &'a self,
        viewer: &'a Viewer,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<ViewerNotification>, AppError>> {
        Box::pin(notifications::recent_for_viewer_pooled(
            &self.pool,
            viewer.clone(),
            limit,
        ))
    }

    fn unread_notifications<'a>(
        &'a self,
        viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<u64, AppError>> {
        Box::pin(notifications::unread_count_pooled(
            &self.pool,
            viewer.clone(),
        ))
    }

    fn mark_notifications_seen<'a>(
        &'a self,
        email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(notifications::mark_seen_pooled(&self.pool, email.clone()))
    }
}
