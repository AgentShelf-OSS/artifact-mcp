//! Collection operations and gallery preferences, independent of storage and transport.

use super::BoxFuture;
use crate::{error::AppError, security::audit::MutationAudit};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_COLLECTIONS_PER_ORG: usize = 200;
pub const MAX_MEMBERSHIPS_PER_REQUEST: usize = 100;
pub const MAX_MEMBERSHIPS_PER_COLLECTION: usize = 1000;
pub const MAX_PREFERENCE_IDS: usize = 2000;
pub const MAX_NAME_LENGTH: usize = 80;
pub const MAX_DESCRIPTION_LENGTH: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CollectionPrincipal {
    HumanEmail(String),
    ApiKeyClient(String),
    OAuthClient { issuer: String, client_id: String },
}

impl CollectionPrincipal {
    pub fn kind_id(&self) -> (&'static str, String) {
        match self {
            Self::HumanEmail(value) => ("email", value.to_lowercase()),
            Self::ApiKeyClient(value) => ("api_key", value.clone()),
            Self::OAuthClient { issuer, client_id } => (
                "oauth",
                serde_json::to_string(&[issuer, client_id]).unwrap_or_default(),
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionActor {
    pub email: String,
    pub org: String,
    pub is_admin: bool,
    pub principal: CollectionPrincipal,
    pub publisher: Option<CollectionPublisher>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Collection {
    pub id: String,
    pub org: String,
    pub name: String,
    pub description: String,
    pub color: Option<String>,
    pub cover_artifact_id: Option<String>,
    pub created_by: String,
    #[serde(skip)]
    pub created_by_kind: String,
    pub created_at: String,
    pub updated_at: String,
    pub artifact_count: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CollectionUpdate {
    pub org: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub color: Option<String>,
    pub cover_artifact_id: Option<String>,
    #[serde(skip)]
    pub clear_cover: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GalleryPreferences {
    pub view: String,
    pub preview_size: String,
    pub artifact_layout: String,
    pub collection_order_by_org: BTreeMap<String, Vec<String>>,
    pub collapsed_collection_ids_by_org: BTreeMap<String, Vec<String>>,
}

impl Default for GalleryPreferences {
    fn default() -> Self {
        Self {
            view: "reel".into(),
            preview_size: "compact".into(),
            artifact_layout: "grid".into(),
            collection_order_by_org: BTreeMap::new(),
            collapsed_collection_ids_by_org: BTreeMap::new(),
        }
    }
}

/// Publisher facts used by collection policy, independent of transport authentication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionPublisher {
    pub org: String,
    pub client_id: String,
    pub role: crate::model::Role,
    pub is_admin: bool,
    pub can_publish: bool,
}

/// Artifact IDs whose memberships changed, followed by IDs whose memberships were unchanged.
pub type MembershipDiff = (Vec<String>, Vec<String>);

/// Fields for creating a collection and its initial memberships atomically.
#[derive(Clone, Debug)]
pub struct NewCollection {
    /// Stable identifier of the new collection.
    pub id: String,
    /// Display name, unique within the organization.
    pub name: String,
    /// Description shown alongside the collection.
    pub description: String,
    /// Optional display color.
    pub color: Option<String>,
    /// Optional cover artifact, which must be an initial member.
    pub cover: Option<String>,
    /// Artifact IDs to include as initial members.
    pub artifact_ids: Vec<String>,
}

/// Organization-scoped collection operations.
pub trait CollectionService: Send + Sync {
    fn list<'a>(
        &'a self,
        actor: &'a CollectionActor,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Collection>, AppError>>;
    fn list_for_org<'a>(
        &'a self,
        actor: &'a CollectionActor,
        org: String,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Collection>, AppError>>;
    fn create_atomic<'a>(
        &'a self,
        actor: &'a CollectionActor,
        collection: NewCollection,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<Collection, AppError>>;
    fn members<'a>(
        &'a self,
        actor: &'a CollectionActor,
        collection_id: String,
    ) -> BoxFuture<'a, Result<Vec<String>, AppError>>;
    fn readable_members<'a>(
        &'a self,
        actor: &'a CollectionActor,
        collection_id: String,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, AppError>>;
    fn add_memberships_audited<'a>(
        &'a self,
        actor: &'a CollectionActor,
        collection_id: String,
        artifact_ids: Vec<String>,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<MembershipDiff, AppError>>;
    fn remove_memberships_audited<'a>(
        &'a self,
        actor: &'a CollectionActor,
        collection_id: String,
        artifact_ids: Vec<String>,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<MembershipDiff, AppError>>;
    fn update_audited<'a>(
        &'a self,
        actor: &'a CollectionActor,
        collection_id: String,
        update: CollectionUpdate,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<(), AppError>>;
    fn delete_audited<'a>(
        &'a self,
        actor: &'a CollectionActor,
        collection_id: String,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<(), AppError>>;
    fn preferences<'a>(
        &'a self,
        actor: &'a CollectionActor,
    ) -> BoxFuture<'a, Result<GalleryPreferences, AppError>>;
    fn save_preferences<'a>(
        &'a self,
        actor: &'a CollectionActor,
        preferences: GalleryPreferences,
    ) -> BoxFuture<'a, Result<GalleryPreferences, AppError>>;
}

/// Explicit unavailable adapter for compositions without a collection database.
pub struct InertCollections;

impl CollectionService for InertCollections {
    fn list<'a>(
        &'a self,
        _: &'a CollectionActor,
        _: usize,
    ) -> BoxFuture<'a, Result<Vec<Collection>, AppError>> {
        Box::pin(async { Err(AppError::Unavailable("database unavailable".into())) })
    }
    fn list_for_org<'a>(
        &'a self,
        _: &'a CollectionActor,
        _: String,
        _: usize,
    ) -> BoxFuture<'a, Result<Vec<Collection>, AppError>> {
        Box::pin(async { Err(AppError::Unavailable("database unavailable".into())) })
    }
    fn create_atomic<'a>(
        &'a self,
        _: &'a CollectionActor,
        _: NewCollection,
        _: MutationAudit,
    ) -> BoxFuture<'a, Result<Collection, AppError>> {
        Box::pin(async { Err(AppError::Unavailable("database unavailable".into())) })
    }
    fn members<'a>(
        &'a self,
        _: &'a CollectionActor,
        _: String,
    ) -> BoxFuture<'a, Result<Vec<String>, AppError>> {
        Box::pin(async { Err(AppError::Unavailable("database unavailable".into())) })
    }
    fn readable_members<'a>(
        &'a self,
        _: &'a CollectionActor,
        _: String,
        _: usize,
        _: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, AppError>> {
        Box::pin(async { Err(AppError::Unavailable("database unavailable".into())) })
    }
    fn add_memberships_audited<'a>(
        &'a self,
        _: &'a CollectionActor,
        _: String,
        _: Vec<String>,
        _: MutationAudit,
    ) -> BoxFuture<'a, Result<MembershipDiff, AppError>> {
        Box::pin(async { Err(AppError::Unavailable("database unavailable".into())) })
    }
    fn remove_memberships_audited<'a>(
        &'a self,
        _: &'a CollectionActor,
        _: String,
        _: Vec<String>,
        _: MutationAudit,
    ) -> BoxFuture<'a, Result<MembershipDiff, AppError>> {
        Box::pin(async { Err(AppError::Unavailable("database unavailable".into())) })
    }
    fn update_audited<'a>(
        &'a self,
        _: &'a CollectionActor,
        _: String,
        _: CollectionUpdate,
        _: MutationAudit,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async { Err(AppError::Unavailable("database unavailable".into())) })
    }
    fn delete_audited<'a>(
        &'a self,
        _: &'a CollectionActor,
        _: String,
        _: MutationAudit,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async { Err(AppError::Unavailable("database unavailable".into())) })
    }
    fn preferences<'a>(
        &'a self,
        _: &'a CollectionActor,
    ) -> BoxFuture<'a, Result<GalleryPreferences, AppError>> {
        Box::pin(async { Err(AppError::Unavailable("database unavailable".into())) })
    }
    fn save_preferences<'a>(
        &'a self,
        _: &'a CollectionActor,
        _: GalleryPreferences,
    ) -> BoxFuture<'a, Result<GalleryPreferences, AppError>> {
        Box::pin(async { Err(AppError::Unavailable("database unavailable".into())) })
    }
}
