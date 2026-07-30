//! Owned by U01 (sol) — reactions, feedback, shares, views, and notifications.

use serde::{Deserialize, Serialize};

use super::{ArtifactId, EmailAddress, FeedbackId, OrgId, ShareToken, Timestamp};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub favorite: i8,
    pub vote: i8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReactionUpdate {
    pub favorite: Option<bool>,
    pub vote: Option<i8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sentiment {
    pub up: u64,
    pub down: u64,
    pub favorites: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewCounts {
    pub views: u64,
    pub unique_viewers: u64,
    pub last_viewed_at: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewerView {
    pub email: EmailAddress,
    pub count: u64,
    pub first_viewed_at: Timestamp,
    pub last_viewed_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopViewedArtifact {
    pub artifact_id: ArtifactId,
    pub title: String,
    pub views: u64,
    pub unique_viewers: u64,
    pub last_viewed_at: Timestamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FeedbackAnchor {
    pub x: f64,
    pub y: f64,
    pub w: Option<f64>,
    pub h: Option<f64>,
    pub approx: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Feedback {
    pub id: FeedbackId,
    pub artifact_id: ArtifactId,
    pub org: OrgId,
    pub parent_id: Option<FeedbackId>,
    pub viewer_email: Option<EmailAddress>,
    pub author: FeedbackAuthor,
    pub body: String,
    pub artifact_revision: u64,
    pub anchor_path: Option<String>,
    pub anchor_x: Option<f64>,
    pub anchor_y: Option<f64>,
    pub anchor_w: Option<f64>,
    pub anchor_h: Option<f64>,
    pub anchor_approx: bool,
    pub anchor_page: Option<String>,
    pub created_at: Timestamp,
    pub resolved_at: Option<Timestamp>,
    pub resolved_by: Option<String>,
    pub external_created_at: Option<Timestamp>,
    pub external_edited_at: Option<Timestamp>,
    pub external_deleted_at: Option<Timestamp>,
}

/// The author identity carried by new feedback projections.
///
/// `Artifact` deliberately retains the historical verified Access email.  `Discord` has no
/// email field: a Discord snowflake or display name must never become an Artifact MCP viewer,
/// owner, or unread-notification identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum FeedbackAuthor {
    Artifact {
        viewer_email: EmailAddress,
    },
    Discord {
        external_author_id: String,
        external_author_display: String,
    },
}

impl FeedbackAuthor {
    /// Backward-compatible projection for legacy callers.  Only verified Artifact identities
    /// produce a viewer email.
    #[must_use]
    pub fn verified_viewer_email(&self) -> Option<&EmailAddress> {
        match self {
            Self::Artifact { viewer_email } => Some(viewer_email),
            Self::Discord { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubmitFeedback {
    pub viewer_email: EmailAddress,
    pub body: String,
    pub parent_id: Option<FeedbackId>,
    pub anchor: Option<FeedbackAnchor>,
    pub anchor_path: Option<String>,
    pub anchor_page: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedbackMutation {
    pub id: FeedbackId,
    pub changed: bool,
}

/// Minimal feedback metadata safe to load before artifact authorization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedbackRef {
    pub id: FeedbackId,
    pub artifact_id: ArtifactId,
    pub org: OrgId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareGrant {
    pub artifact_id: ArtifactId,
    pub org: OrgId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicShare {
    pub token: ShareToken,
    pub expires_at: Option<Timestamp>,
    pub created_at: Option<Timestamp>,
    pub created_by: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateShare {
    pub created_by: String,
    pub expires: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewerNotification {
    pub id: FeedbackId,
    pub artifact_id: ArtifactId,
    pub artifact_title: String,
    pub org: OrgId,
    pub body: String,
    pub author: FeedbackAuthor,
    pub created_at: Timestamp,
    pub parent_id: Option<FeedbackId>,
    pub resolved: bool,
    pub has_anchor: bool,
    pub unread: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationPayload {
    pub artifact_id: ArtifactId,
    pub title: String,
    pub url: String,
    pub description: String,
    pub uploader_label: String,
    pub category: String,
    pub revision: u64,
    pub bytes: u64,
    pub viewer_email: Option<EmailAddress>,
    pub body: Option<String>,
    pub resolver: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryResult {
    pub ok: bool,
    pub error: Option<String>,
}
