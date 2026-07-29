//! Owned by U01 (sol) — frozen renderer input models.

use std::collections::BTreeMap;

use crate::{
    model::{
        ArtifactId, OrgArtifacts, OrgId, Organization, PublisherKeySummary, Reaction, Sentiment,
        TopViewedArtifact, ViewCounts, Viewer, ViewerNotification, ViewerView, WebhookSummary,
    },
    security::access::AuthorizedArtifact,
};

#[derive(Clone, Debug)]
pub struct GalleryView {
    pub viewer: Viewer,
    pub sections: Vec<OrgArtifacts>,
    pub reactions: BTreeMap<ArtifactId, Reaction>,
    pub sentiment: BTreeMap<ArtifactId, Sentiment>,
    pub view_counts: BTreeMap<ArtifactId, ViewCounts>,
    pub top_viewed: BTreeMap<OrgId, Vec<TopViewedArtifact>>,
    pub org_colors: BTreeMap<OrgId, Option<String>>,
    pub notifications: Vec<ViewerNotification>,
    pub unread_notifications: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtifactNavigation {
    pub previous_id: Option<ArtifactId>,
    pub next_id: Option<ArtifactId>,
    pub index: usize,
    pub total: usize,
}

#[derive(Clone, Debug)]
pub struct ShellView {
    pub artifact: AuthorizedArtifact,
    pub navigation: ArtifactNavigation,
    pub reaction: Reaction,
    pub feedback: Vec<crate::model::Feedback>,
    pub view_counts: ViewCounts,
    pub viewers: Option<Vec<ViewerView>>,
    pub viewer: Viewer,
    pub org_accent: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SettingsOrganization {
    pub organization: Organization,
    pub webhooks: Vec<WebhookSummary>,
}

#[derive(Clone, Debug)]
pub struct SettingsView {
    pub viewer: Viewer,
    pub keys: Vec<PublisherKeySummary>,
    pub organizations: Vec<SettingsOrganization>,
}
