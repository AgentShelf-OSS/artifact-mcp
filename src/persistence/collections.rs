//! Durable organization collections and gallery presentation preferences.
//!
//! A collection is a reference set.  It never copies an artifact body or changes artifact
//! metadata.  All writes are performed in one SQLite transaction so a bulk collect either
//! succeeds completely or leaves every membership unchanged.

use std::collections::{BTreeMap, HashSet};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::{
    error::AppError,
    persistence::db::{self, DbPool},
    security::audit::{AuditEvent, MutationAudit, append_in_transaction},
};

pub const MAX_COLLECTIONS_PER_ORG: usize = 200;
pub const MAX_MEMBERSHIPS_PER_REQUEST: usize = 100;
pub const MAX_MEMBERSHIPS_PER_COLLECTION: usize = 1000;
pub const MAX_PREFERENCE_IDS: usize = 2000;
pub const MAX_NAME_LENGTH: usize = 80;
pub const MAX_DESCRIPTION_LENGTH: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionActor {
    pub email: String,
    pub org: String,
    pub is_admin: bool,
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

#[derive(Clone)]
pub struct CollectionStore {
    pool: DbPool,
    audit_key: Option<[u8; 32]>,
}

impl CollectionStore {
    #[must_use]
    pub const fn new(pool: DbPool) -> Self {
        Self {
            pool,
            audit_key: None,
        }
    }

    #[must_use]
    pub const fn with_audit(pool: DbPool, audit_key: [u8; 32]) -> Self {
        Self {
            pool,
            audit_key: Some(audit_key),
        }
    }

    fn append_audit(
        audit_key: Option<[u8; 32]>,
        tx: &rusqlite::Transaction<'_>,
        audit: Option<&MutationAudit>,
        operation: &str,
        target_id: &str,
        tenant: &str,
    ) -> Result<(), AppError> {
        let (Some(key), Some(audit)) = (audit_key.as_ref(), audit) else {
            return Ok(());
        };
        let scoped = audit.for_affected_tenant(&crate::model::OrgId(tenant.to_owned()));
        let event = AuditEvent {
            operation: operation.to_owned(),
            target_type: "collection".to_owned(),
            target_id: target_id.to_owned(),
            result: "success".to_owned(),
            classification: "collection_mutation".to_owned(),
            revision: None,
        };
        append_in_transaction(tx, key, &scoped.event_id()?, scoped.context(), &event)?;
        Ok(())
    }

    pub async fn list(
        &self,
        actor: &CollectionActor,
        limit: usize,
    ) -> Result<Vec<Collection>, AppError> {
        let org = actor.org.clone();
        let limit = limit.min(MAX_COLLECTIONS_PER_ORG).max(1) as i64;
        db::interact(&self.pool, move |conn| {
            let mut stmt = conn.prepare(
                "SELECT c.id,c.org,c.name,c.description,c.color,c.cover_artifact_id,c.created_by,c.created_at,c.updated_at,COUNT(m.artifact_id) FROM collections c LEFT JOIN collection_artifacts m ON m.collection_id=c.id WHERE c.org=?1 GROUP BY c.id ORDER BY c.created_at ASC,c.id ASC LIMIT ?2",
            ).map_err(|_| AppError::Internal)?;
            stmt.query_map(params![org, limit], collection_row)
                .map_err(|_| AppError::Internal)?.collect::<Result<Vec<_>, _>>().map_err(|_| AppError::Internal)
        }).await
    }

    pub async fn list_for_org(
        &self,
        actor: &CollectionActor,
        org: String,
        limit: usize,
    ) -> Result<Vec<Collection>, AppError> {
        let mut scoped = actor.clone();
        scoped.org = org;
        self.list(&scoped, limit).await
    }

    pub async fn create(
        &self,
        actor: &CollectionActor,
        id: String,
        name: String,
        description: String,
        color: Option<String>,
        cover: Option<String>,
    ) -> Result<Collection, AppError> {
        self.create_atomic(actor, id, name, description, color, cover, Vec::new(), None)
            .await
    }

    pub async fn create_atomic(
        &self,
        actor: &CollectionActor,
        id: String,
        name: String,
        description: String,
        color: Option<String>,
        cover: Option<String>,
        artifact_ids: Vec<String>,
        audit: Option<MutationAudit>,
    ) -> Result<Collection, AppError> {
        validate_id(&id)?;
        validate_name(&name)?;
        validate_description(&description)?;
        if let Some(color) = &color {
            validate_color(color)?;
        }
        validate_ids(&artifact_ids)?;
        let actor = actor.clone();
        let audit_key = self.audit_key;
        db::interact(&self.pool, move |conn| {
            authorize_org(&actor)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_| AppError::Internal)?;
            let count: i64 = tx.query_row("SELECT COUNT(*) FROM collections WHERE org=?1", [&actor.org], |r| r.get(0)).map_err(|_| AppError::Internal)?;
            if count >= MAX_COLLECTIONS_PER_ORG as i64 { return Err(AppError::Validation("collection_limit".into())); }
            ensure_existing_org(&tx, &actor.org)?;
            if let Some(ref artifact) = cover { ensure_artifact_readable(&tx, artifact, &actor)?; }
            for artifact in &artifact_ids { ensure_artifact_readable(&tx, artifact, &actor)?; }
            if let Some(cover_id) = cover.as_ref() && !artifact_ids.iter().any(|id| id == cover_id) {
                return Err(AppError::Validation("cover artifact must be a collection member".into()));
            }
            let name_key = normalize_name(&name);
            tx.execute("INSERT INTO collections(id,org,name,name_key,description,color,cover_artifact_id,cover_artifact_org,created_by) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![id,actor.org,normalize_text(&name),name_key,normalize_text(&description),color,cover,cover.as_ref().map(|_| actor.org.clone()),actor.email]).map_err(|e| if matches!(e, rusqlite::Error::SqliteFailure(_, _)) { AppError::Conflict("a collection with that name already exists".into()) } else { AppError::Internal })?;
            for artifact_id in &artifact_ids {
                tx.execute("INSERT INTO collection_artifacts(collection_id,artifact_id,org) VALUES (?1,?2,?3)", params![id, artifact_id, actor.org]).map_err(|_| AppError::Internal)?;
            }
            Self::append_audit(audit_key, &tx, audit.as_ref(), "collection.create", &id, &actor.org)?;
            let result = tx.query_row("SELECT c.id,c.org,c.name,c.description,c.color,c.cover_artifact_id,c.created_by,c.created_at,c.updated_at,COUNT(m.artifact_id) FROM collections c LEFT JOIN collection_artifacts m ON m.collection_id=c.id WHERE c.id=?1 GROUP BY c.id", [&id], collection_row).map_err(|_| AppError::Internal)?;
            tx.commit().map_err(|_| AppError::Internal)?; Ok(result)
        }).await
    }

    pub async fn members(
        &self,
        actor: &CollectionActor,
        collection_id: String,
    ) -> Result<Vec<String>, AppError> {
        validate_id(&collection_id)?;
        let actor = actor.clone();
        db::interact(&self.pool, move |conn| {
            let org: Option<String> = conn.query_row("SELECT org FROM collections WHERE id=?1", [&collection_id], |r| r.get(0)).optional().map_err(|_| AppError::Internal)?;
            let Some(org) = org else { return Err(AppError::NotFound("Collection not found".into())); };
            if org != actor.org { return Err(AppError::ConcealedNotFound); }
            let mut stmt = conn.prepare("SELECT artifact_id FROM collection_artifacts WHERE collection_id=?1 AND org=?2 ORDER BY created_at ASC,artifact_id ASC LIMIT 1000").map_err(|_| AppError::Internal)?;
            stmt.query_map(params![collection_id,actor.org], |r| r.get(0)).map_err(|_| AppError::Internal)?.collect::<Result<Vec<String>, _>>().map_err(|_| AppError::Internal)
        }).await
    }

    pub async fn add_memberships(
        &self,
        actor: &CollectionActor,
        collection_id: String,
        artifact_ids: Vec<String>,
    ) -> Result<usize, AppError> {
        self.add_memberships_audited(actor, collection_id, artifact_ids, None)
            .await
    }

    pub async fn add_memberships_audited(
        &self,
        actor: &CollectionActor,
        collection_id: String,
        artifact_ids: Vec<String>,
        audit: Option<MutationAudit>,
    ) -> Result<usize, AppError> {
        validate_id(&collection_id)?;
        validate_ids(&artifact_ids)?;
        let actor = actor.clone();
        let audit_key = self.audit_key;
        db::interact(&self.pool, move |conn| {
            authorize_org(&actor)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_| AppError::Internal)?;
            ensure_collection_creator(&tx, &collection_id, &actor)?;
            for id in &artifact_ids { ensure_artifact_readable(&tx, id, &actor)?; }
            let count: i64 = tx.query_row("SELECT COUNT(*) FROM collection_artifacts WHERE collection_id=?1", [&collection_id], |r| r.get(0)).map_err(|_| AppError::Internal)?;
            let mut extra = 0;
            for id in &artifact_ids {
                let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM collection_artifacts WHERE collection_id=?1 AND artifact_id=?2)", params![collection_id,id], |r|r.get(0)).map_err(|_|AppError::Internal)?;
                if !exists { extra += 1; }
            }
            if count + extra > MAX_MEMBERSHIPS_PER_COLLECTION as i64 { return Err(AppError::Validation("collection member limit reached".into())); }
            let mut added = 0;
            for id in &artifact_ids { added += tx.execute("INSERT OR IGNORE INTO collection_artifacts(collection_id,artifact_id,org) VALUES (?1,?2,?3)", params![collection_id,id,actor.org]).map_err(|_| AppError::Internal)?; }
            tx.execute("UPDATE collections SET updated_at=datetime('now') WHERE id=?1", [&collection_id]).map_err(|_| AppError::Internal)?;
            Self::append_audit(audit_key, &tx, audit.as_ref(), "collection.membership.add", &collection_id, &actor.org)?;
            tx.commit().map_err(|_| AppError::Internal)?; Ok(added)
        }).await
    }

    pub async fn remove_memberships(
        &self,
        actor: &CollectionActor,
        collection_id: String,
        artifact_ids: Vec<String>,
    ) -> Result<usize, AppError> {
        self.remove_memberships_audited(actor, collection_id, artifact_ids, None)
            .await
    }

    pub async fn remove_memberships_audited(
        &self,
        actor: &CollectionActor,
        collection_id: String,
        artifact_ids: Vec<String>,
        audit: Option<MutationAudit>,
    ) -> Result<usize, AppError> {
        validate_id(&collection_id)?;
        validate_ids(&artifact_ids)?;
        let actor = actor.clone();
        let audit_key = self.audit_key;
        db::interact(&self.pool, move |conn| {
            authorize_org(&actor)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_| AppError::Internal)?;
            ensure_collection_creator(&tx, &collection_id, &actor)?;
            for id in &artifact_ids { ensure_artifact_readable(&tx, id, &actor)?; }
            let mut removed = 0;
            for id in &artifact_ids { removed += tx.execute("DELETE FROM collection_artifacts WHERE collection_id=?1 AND artifact_id=?2", params![collection_id,id]).map_err(|_| AppError::Internal)?; }
            if let Some(cover) = tx.query_row("SELECT cover_artifact_id FROM collections WHERE id=?1", [&collection_id], |r| r.get::<_, Option<String>>(0)).optional().map_err(|_| AppError::Internal)?.flatten() && artifact_ids.iter().any(|id| id == &cover) {
                tx.execute("UPDATE collections SET cover_artifact_id=NULL, cover_artifact_org=NULL WHERE id=?1", [&collection_id]).map_err(|_| AppError::Internal)?;
            }
            tx.execute("UPDATE collections SET updated_at=datetime('now') WHERE id=?1", [&collection_id]).map_err(|_| AppError::Internal)?;
            Self::append_audit(audit_key, &tx, audit.as_ref(), "collection.membership.remove", &collection_id, &actor.org)?;
            tx.commit().map_err(|_| AppError::Internal)?; Ok(removed)
        }).await
    }

    pub async fn update(
        &self,
        actor: &CollectionActor,
        collection_id: String,
        update: CollectionUpdate,
    ) -> Result<(), AppError> {
        self.update_audited(actor, collection_id, update, None)
            .await
    }

    pub async fn update_audited(
        &self,
        actor: &CollectionActor,
        collection_id: String,
        update: CollectionUpdate,
        audit: Option<MutationAudit>,
    ) -> Result<(), AppError> {
        validate_id(&collection_id)?;
        if let Some(name) = &update.name {
            validate_name(name)?;
        }
        if let Some(description) = &update.description {
            validate_description(description)?;
        }
        if let Some(color) = &update.color {
            if !color.is_empty() {
                validate_color(color)?;
            }
        }
        let actor = actor.clone();
        let audit_key = self.audit_key;
        db::interact(&self.pool, move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_| AppError::Internal)?;
            ensure_collection_creator(&tx, &collection_id, &actor)?;
            if let Some(ref cover) = update.cover_artifact_id { ensure_artifact_readable(&tx, cover, &actor)?; }
            let current: (String, String, Option<String>, Option<String>) = tx.query_row("SELECT name,description,color,cover_artifact_id FROM collections WHERE id=?1", [&collection_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(|_| AppError::Internal)?;
            let name = normalize_text(update.name.as_deref().unwrap_or(&current.0));
            let description = normalize_text(update.description.as_deref().unwrap_or(&current.1));
            let color = match update.color.as_deref() { Some("") | Some("null") => None, Some(value) => Some(value.to_owned()), None => current.2 };
            let cover = if update.clear_cover { None } else { update.cover_artifact_id.or(current.3) };
            if let Some(cover_id) = cover.as_ref() {
                let member: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM collection_artifacts WHERE collection_id=?1 AND artifact_id=?2)", params![collection_id, cover_id], |r| r.get(0)).map_err(|_| AppError::Internal)?;
                if !member { return Err(AppError::Validation("cover artifact must be a collection member".into())); }
            }
            tx.execute("UPDATE collections SET name=?1,name_key=?2,description=?3,color=?4,cover_artifact_id=?5,cover_artifact_org=?6,updated_at=datetime('now') WHERE id=?7", params![name,normalize_name(&name),description,color,cover,cover.as_ref().map(|_| actor.org.clone()),collection_id]).map_err(|e| if matches!(e, rusqlite::Error::SqliteFailure(_, _)) { AppError::Conflict("a collection with that name already exists".into()) } else { AppError::Internal })?;
            Self::append_audit(audit_key, &tx, audit.as_ref(), "collection.update", &collection_id, &actor.org)?;
            tx.commit().map_err(|_| AppError::Internal)
        }).await
    }

    pub async fn delete(
        &self,
        actor: &CollectionActor,
        collection_id: String,
    ) -> Result<(), AppError> {
        self.delete_audited(actor, collection_id, None).await
    }

    pub async fn delete_audited(
        &self,
        actor: &CollectionActor,
        collection_id: String,
        audit: Option<MutationAudit>,
    ) -> Result<(), AppError> {
        validate_id(&collection_id)?;
        let actor = actor.clone();
        let audit_key = self.audit_key;
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            ensure_collection_creator(&tx, &collection_id, &actor)?;
            tx.execute("DELETE FROM collections WHERE id=?1", [&collection_id])
                .map_err(|_| AppError::Internal)?;
            Self::append_audit(
                audit_key,
                &tx,
                audit.as_ref(),
                "collection.delete",
                &collection_id,
                &actor.org,
            )?;
            tx.commit().map_err(|_| AppError::Internal)
        })
        .await
    }

    pub async fn preferences(
        &self,
        actor: &CollectionActor,
    ) -> Result<GalleryPreferences, AppError> {
        authorize_org(actor)?;
        let actor = actor.clone();
        db::interact(&self.pool, move |conn| {
            let mut preferences = load_preferences(conn, &actor.email.to_lowercase())?;
            prune_preferences(conn, &actor, &mut preferences)?;
            Ok(preferences)
        })
        .await
    }

    pub async fn save_preferences(
        &self,
        actor: &CollectionActor,
        preferences: GalleryPreferences,
    ) -> Result<GalleryPreferences, AppError> {
        authorize_org(actor)?;
        validate_preferences(&preferences)?;
        let actor = actor.clone();
        db::interact(&self.pool, move |conn| {
            let mut normalized = preferences;
            // Writes retain all authorized admin scopes. GET can project one scope.
            let mut write_actor = actor.clone();
            if write_actor.is_admin { write_actor.org = "all".into(); }
            prune_preferences(conn, &write_actor, &mut normalized)?;
            conn.execute("INSERT INTO gallery_preferences(viewer_email,view,preview_size,artifact_layout,state_json,updated_at) VALUES (?1,?2,?3,?4,?5,datetime('now')) ON CONFLICT(viewer_email) DO UPDATE SET view=excluded.view,preview_size=excluded.preview_size,artifact_layout=excluded.artifact_layout,state_json=excluded.state_json,updated_at=datetime('now')", params![actor.email.to_lowercase(),normalized.view,normalized.preview_size,normalized.artifact_layout,serde_json::json!({"collectionOrderByOrg":normalized.collection_order_by_org,"collapsedCollectionIdsByOrg":normalized.collapsed_collection_ids_by_org}).to_string()]).map_err(|_| AppError::Internal)?;
            Ok(normalized)
        }).await
    }
}

fn authorize_org(actor: &CollectionActor) -> Result<(), AppError> {
    if actor.email.trim().is_empty() || actor.org.trim().is_empty() {
        return Err(AppError::Unauthorized("Not signed in".into()));
    }
    Ok(())
}

fn ensure_collection_creator(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    actor: &CollectionActor,
) -> Result<(), AppError> {
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT org,created_by FROM collections WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|_| AppError::Internal)?;
    let Some((org, created_by)) = row else {
        return Err(AppError::NotFound("Collection not found".into()));
    };
    if org != actor.org {
        return Err(AppError::ConcealedNotFound);
    }
    if actor.is_admin || created_by.eq_ignore_ascii_case(&actor.email) {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "You cannot modify this collection".into(),
        ))
    }
}

fn ensure_existing_org(conn: &Connection, org: &str) -> Result<(), AppError> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM orgs WHERE name=?1)",
            [org],
            |r| r.get(0),
        )
        .map_err(|_| AppError::Internal)?;
    if exists {
        Ok(())
    } else {
        Err(AppError::ConcealedNotFound)
    }
}

fn ensure_artifact_readable(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    actor: &CollectionActor,
) -> Result<(), AppError> {
    let found: Option<(String, bool, Option<String>)> = tx
        .query_row(
            "SELECT org,hidden,owner_email FROM artifacts WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|_| AppError::Internal)?;
    match found {
        Some((org, hidden, owner))
            if org == actor.org
                && (actor.is_admin
                    || !hidden
                    || owner
                        .as_deref()
                        .is_some_and(|email| email.eq_ignore_ascii_case(&actor.email))) =>
        {
            Ok(())
        }
        _ => Err(AppError::ConcealedNotFound),
    }
}

fn prune_preferences(
    conn: &Connection,
    actor: &CollectionActor,
    preferences: &mut GalleryPreferences,
) -> Result<(), AppError> {
    let mut stmt = conn
        .prepare("SELECT org,id FROM collections ORDER BY org,created_at,id")
        .map_err(|_| AppError::Internal)?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|_| AppError::Internal)?;
    let mut allowed: std::collections::BTreeMap<String, HashSet<String>> =
        std::collections::BTreeMap::new();
    let mut all = HashSet::new();
    for row in rows {
        let (org, id) = row.map_err(|_| AppError::Internal)?;
        if actor.is_admin && actor.org == "all" || org == actor.org {
            all.insert(id.clone());
            allowed.entry(org).or_default().insert(id);
        }
    }
    if actor.is_admin && actor.org == "all" {
        allowed.insert("all".into(), all);
    }
    for map in [
        &mut preferences.collection_order_by_org,
        &mut preferences.collapsed_collection_ids_by_org,
    ] {
        map.retain(|org, ids| {
            let Some(valid) = allowed.get(org) else {
                return false;
            };
            let mut seen = HashSet::new();
            ids.retain(|id| valid.contains(id) && seen.insert(id.clone()));
            true
        });
    }
    Ok(())
}

fn collection_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Collection> {
    Ok(Collection {
        id: row.get(0)?,
        org: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        color: row.get(4)?,
        cover_artifact_id: row.get(5)?,
        created_by: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        artifact_count: row.get::<_, i64>(9)? as u64,
    })
}

fn load_preferences(conn: &Connection, email: &str) -> Result<GalleryPreferences, AppError> {
    let row: Option<(String,String,String,String)> = conn.query_row("SELECT view,preview_size,artifact_layout,state_json FROM gallery_preferences WHERE viewer_email=?1", [email], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|_| AppError::Internal)?;
    let Some((view_mode, preview_size, artifact_layout, state)) = row else {
        return Ok(GalleryPreferences::default());
    };
    let state: serde_json::Value = serde_json::from_str(&state).map_err(|_| AppError::Internal)?;
    let collection_order_by_org = state
        .get("collectionOrderByOrg")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| AppError::Internal)?
        .unwrap_or_default();
    let collapsed_collection_ids_by_org = state
        .get("collapsedCollectionIdsByOrg")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| AppError::Internal)?
        .unwrap_or_default();
    let result = GalleryPreferences {
        view: view_mode,
        preview_size,
        artifact_layout,
        collection_order_by_org,
        collapsed_collection_ids_by_org,
    };
    validate_preferences(&result)?;
    Ok(result)
}

fn validate_id(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Err(AppError::Validation("invalid collection id".into()))
    } else {
        Ok(())
    }
}
fn validate_name(value: &str) -> Result<(), AppError> {
    let value = normalize_text(value);
    if value.is_empty() {
        return Err(AppError::Validation("name is required".into()));
    }
    if value.chars().count() > MAX_NAME_LENGTH {
        Err(AppError::Validation(
            "collection name must be 1–80 characters".into(),
        ))
    } else {
        Ok(())
    }
}
fn normalize_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn normalize_name(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
fn validate_description(value: &str) -> Result<(), AppError> {
    if normalize_text(value).chars().count() > MAX_DESCRIPTION_LENGTH {
        Err(AppError::Validation(
            "collection description is too long".into(),
        ))
    } else {
        Ok(())
    }
}
fn validate_color(value: &str) -> Result<(), AppError> {
    if value.len() != 7
        || !value.starts_with('#')
        || !value[1..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        Err(AppError::Validation("invalid collection color".into()))
    } else {
        Ok(())
    }
}
fn validate_ids(ids: &[String]) -> Result<(), AppError> {
    if ids.len() > MAX_MEMBERSHIPS_PER_REQUEST {
        return Err(AppError::Validation("invalid_ids".into()));
    }
    let mut seen = HashSet::new();
    for id in ids {
        validate_id(id).map_err(|_| AppError::Validation("invalid_ids".into()))?;
        if !seen.insert(id) {
            return Err(AppError::Validation("duplicate artifact id".into()));
        }
    }
    Ok(())
}
fn validate_preferences(value: &GalleryPreferences) -> Result<(), AppError> {
    if !matches!(value.view.as_str(), "reel" | "sheets" | "ribbons" | "all")
        || !matches!(value.preview_size.as_str(), "compact" | "large")
        || !matches!(value.artifact_layout.as_str(), "grid" | "list")
    {
        return Err(AppError::Validation("invalid gallery preferences".into()));
    }
    for ids in value
        .collection_order_by_org
        .values()
        .chain(value.collapsed_collection_ids_by_org.values())
    {
        if ids.len() > MAX_PREFERENCE_IDS {
            return Err(AppError::Validation(
                "preference lists are too large".into(),
            ));
        }
        for id in ids {
            validate_id(id)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_collection_inputs() {
        assert!(validate_name(" Design ").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_color("#e4d3b4").is_ok());
        assert!(validate_color("red").is_err());
    }
    #[test]
    fn defaults_are_stable() {
        assert_eq!(GalleryPreferences::default().view, "reel");
    }
}
