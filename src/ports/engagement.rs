//! Owned by U01 (sol) — authorization-gated engagement and sharing contracts.

use std::collections::BTreeMap;

use super::BoxFuture;
use crate::{
    error::AppError,
    model::{
        ArtifactId, CreateShare, EmailAddress, Feedback, FeedbackId, FeedbackMutation, FeedbackRef,
        OrgId, PublicShare, PublisherIdentity, Reaction, ReactionUpdate, Sentiment, ShareGrant,
        ShareToken, SubmitFeedback, TopViewedArtifact, ViewCounts, Viewer, ViewerNotification,
        ViewerView,
    },
    security::access::{AuthorizedArtifact, OwnedArtifact},
};

pub trait EngagementService: Send + Sync {
    fn reaction<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<Reaction, AppError>>;
    fn set_reaction(
        &self,
        artifact: AuthorizedArtifact,
        viewer: Viewer,
        update: ReactionUpdate,
    ) -> BoxFuture<'_, Result<Reaction, AppError>>;
    fn reactions_for_viewer<'a>(
        &'a self,
        viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<BTreeMap<ArtifactId, Reaction>, AppError>>;
    fn sentiment(&self) -> BoxFuture<'_, Result<BTreeMap<ArtifactId, Sentiment>, AppError>>;

    fn record_view<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<(), AppError>>;
    fn view_counts<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<ViewCounts, AppError>>;
    fn view_counts_for_org<'a>(
        &'a self,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<BTreeMap<ArtifactId, ViewCounts>, AppError>>;
    fn viewers<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Vec<ViewerView>, AppError>>;
    fn top_for_org<'a>(
        &'a self,
        org: &'a OrgId,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<TopViewedArtifact>, AppError>>;

    fn feedback_ref<'a>(
        &'a self,
        id: &'a FeedbackId,
    ) -> BoxFuture<'a, Result<Option<FeedbackRef>, AppError>>;
    fn list_feedback<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Vec<Feedback>, AppError>>;
    fn submit_feedback(
        &self,
        artifact: AuthorizedArtifact,
        submission: SubmitFeedback,
    ) -> BoxFuture<'_, Result<Feedback, AppError>>;
    fn delete_feedback(
        &self,
        artifact: AuthorizedArtifact,
        viewer: Viewer,
        id: FeedbackId,
    ) -> BoxFuture<'_, Result<FeedbackMutation, AppError>>;
    fn resolve_feedback_as_viewer(
        &self,
        artifact: AuthorizedArtifact,
        viewer: Viewer,
        id: FeedbackId,
    ) -> BoxFuture<'_, Result<FeedbackMutation, AppError>>;
    fn list_feedback_for_publisher<'a>(
        &'a self,
        publisher: &'a PublisherIdentity,
        artifact: Option<&'a OwnedArtifact>,
    ) -> BoxFuture<'a, Result<Vec<Feedback>, AppError>>;
    fn resolve_feedback_as_publisher(
        &self,
        artifact: OwnedArtifact,
        id: FeedbackId,
        resolved_by: String,
    ) -> BoxFuture<'_, Result<bool, AppError>>;
    fn reopen_feedback_as_publisher(
        &self,
        artifact: OwnedArtifact,
        id: FeedbackId,
    ) -> BoxFuture<'_, Result<bool, AppError>>;

    fn recent_notifications<'a>(
        &'a self,
        viewer: &'a Viewer,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<ViewerNotification>, AppError>>;
    fn unread_notifications<'a>(
        &'a self,
        viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<u64, AppError>>;
    fn mark_notifications_seen<'a>(
        &'a self,
        email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<(), AppError>>;
}

pub trait ShareService: Send + Sync {
    /// Resolve only the artifact/org grant; body access still requires U06 policy conversion.
    fn resolve<'a>(
        &'a self,
        token: &'a ShareToken,
    ) -> BoxFuture<'a, Result<Option<ShareGrant>, AppError>>;
    fn create(
        &self,
        artifact: AuthorizedArtifact,
        request: CreateShare,
    ) -> BoxFuture<'_, Result<PublicShare, AppError>>;
    fn list<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Vec<PublicShare>, AppError>>;
    fn revoke(
        &self,
        artifact: AuthorizedArtifact,
        token: ShareToken,
    ) -> BoxFuture<'_, Result<bool, AppError>>;
}
