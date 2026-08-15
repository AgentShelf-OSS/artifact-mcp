//! Owned by U01 (sol) — administrator, organization, key, and webhook records.

use serde::{Deserialize, Serialize};

use super::{ClientId, OrgId, Timestamp, WebhookId};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublisherKeySummary {
    pub client_id: ClientId,
    pub org: OrgId,
    pub label: String,
    pub role: String,
    /// Administrator-visible key attribution; absent for service/shared keys.
    pub owner_email: Option<String>,
    pub created_at: Timestamp,
    pub revoked_at: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePublisherKey {
    pub client_id: ClientId,
    pub org: OrgId,
    pub label: String,
    pub role: String,
    /// Optional, verified same-org member identity.  Never accepted by MCP publish calls.
    pub owner_email: Option<String>,
}

/// Administrator-editable metadata for an existing publisher credential.
///
/// The credential identity and tenant are deliberately absent: changing either would break the
/// publish-time attribution snapshots held by existing artifacts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePublisherKey {
    pub label: String,
    pub role: String,
    pub owner_email: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatedPublisherKey {
    pub client_id: ClientId,
    pub org: OrgId,
    pub label: String,
    pub role: String,
    pub owner_email: Option<String>,
}

/// A newly minted publisher key, including the one-time plaintext secret.
///
/// `Debug` is implemented by hand to REDACT `secret`. The derived implementation printed the live
/// key in the clear, so any `tracing` event, `assert!` message, or `{:?}` in an error path would
/// have leaked a working credential (proven empirically before the fix). `Serialize` is retained
/// deliberately — the secret is displayed exactly once at creation, which is the whole point of the
/// type — so callers must keep serialized values off logs.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedPublisherKey {
    pub client_id: ClientId,
    pub org: OrgId,
    pub label: String,
    pub role: String,
    pub owner_email: Option<String>,
    pub secret: String,
}

/// Result of an admin key-owner change. Existing artifacts intentionally do not participate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyOwnerUpdate {
    pub client_id: ClientId,
    pub org: OrgId,
    pub owner_email: Option<String>,
}

/// Preview/confirmation result for bounded legacy owner attribution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerBackfillResult {
    pub client_id: ClientId,
    pub org: OrgId,
    pub owner_email: String,
    pub matched: u64,
    pub updated: u64,
    pub confirmed: bool,
}

impl std::fmt::Debug for CreatedPublisherKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatedPublisherKey")
            .field("client_id", &self.client_id)
            .field("org", &self.org)
            .field("label", &self.label)
            .field("role", &self.role)
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Organization {
    pub name: OrgId,
    pub label: String,
    pub color: Option<String>,
    pub created_at: Option<Timestamp>,
    pub domains: Vec<String>,
    pub emails: Vec<String>,
    pub categories: Vec<String>,
    pub key_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateOrganization {
    pub name: OrgId,
    pub label: String,
    pub domain: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookEvent {
    Published,
    Updated,
    Restored,
    Deleted,
    Feedback,
    Resolved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookSummary {
    pub id: WebhookId,
    pub label: String,
    pub events: Vec<WebhookEvent>,
    pub url: String,
    pub last_ok_at: Option<Timestamp>,
    pub last_error: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct WebhookDelivery {
    pub id: WebhookId,
    pub org: OrgId,
    pub url: String,
    pub label: String,
    pub events: Vec<WebhookEvent>,
}

impl std::fmt::Debug for WebhookDelivery {
    /// Hand-written to REDACT `url`.
    ///
    /// A webhook URL is a bearer credential: anyone holding it can post to the channel. The derived
    /// implementation meant a single `tracing::debug!(?delivery)` in any future caller would leak a
    /// live one. Flagged by U12, which never formats this type but could not fix the U01-frozen
    /// model itself. Same defect class as `CreatedPublisherKey`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookDelivery")
            .field("id", &self.id)
            .field("org", &self.org)
            .field("url", &"<redacted>")
            .field("label", &self.label)
            .field("events", &self.events)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct CreateWebhook {
    pub org: OrgId,
    pub url: String,
    pub label: String,
    pub events: Option<Vec<WebhookEvent>>,
}

impl std::fmt::Debug for CreateWebhook {
    /// Hand-written to REDACT `url` — it carries the raw, unmasked webhook URL straight off the
    /// request, so `tracing::debug!(?request)` in any future route handler would log a live bearer
    /// credential. Third type in this family; see `WebhookDelivery` and `CreatedPublisherKey`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreateWebhook")
            .field("org", &self.org)
            .field("url", &"<redacted>")
            .field("label", &self.label)
            .field("events", &self.events)
            .finish()
    }
}
