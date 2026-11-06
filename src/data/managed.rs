//! Managed-source administration: views, CRUD with audit, impact, and connection tests.
use super::DataError;
use super::{
    BindingManifest, DataBroker, ImpactArtifact, SourceConfig, SourceHealth, SourceView,
    TestResult,
    validate::{build_url, validate_managed_credentials, validate_source},
};
use crate::{
    data::error::interact,
    security::audit::{AuditEvent, MutationAudit, append_in_transaction},
};
use rusqlite::{OptionalExtension, TransactionBehavior};
use serde_json::{Value, json};

const SELECT_MANAGED_SOURCE_VERSION: &str =
    "SELECT org,version,enabled FROM data_sources WHERE id=?1";
const UPDATE_MANAGED_SOURCE: &str = "UPDATE data_sources SET org=?2,definition=?3,enabled=?4,version=version+1,updated_at=datetime('now') WHERE id=?1";

const SOURCE_EXISTS: &str = "SELECT 1 FROM data_sources WHERE id=?1";
const ORG_EXISTS: &str = "SELECT 1 FROM orgs WHERE name=?1";
const INSERT_MANAGED_SOURCE: &str =
    "INSERT INTO data_sources(id,org,definition,enabled,version) VALUES(?1,?2,?3,?4,1)";
const UPDATE_MANAGED_ENABLED: &str = "UPDATE data_sources SET enabled=?2,version=version+1,updated_at=datetime('now') WHERE id=?1 AND version=?3";
const SELECT_SOURCE_VERSION: &str = "SELECT version FROM data_sources WHERE id=?1";
const DELETE_MANAGED_SOURCE: &str = "DELETE FROM data_sources WHERE id=?1";
const SELECT_BOUND_ARTIFACTS: &str = "SELECT a.id,a.title,a.org,b.bindings FROM artifact_data_bindings b JOIN artifacts a ON a.id=b.artifact_id WHERE a.org=b.org";
const SELECT_CURRENT_BINDINGS: &str = "SELECT bindings FROM artifact_data_bindings b JOIN artifacts a ON a.id=b.artifact_id WHERE a.org=b.org";

impl DataBroker {
    pub async fn audit_mutation(
        &self,
        audit: MutationAudit,
        operation: &str,
        id: &str,
        _org: &str,
        result: &str,
        revision: Option<u64>,
    ) -> Result<(), DataError> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };
        let org = self.source_record(id).await?.0.org;
        let audit = audit
            .for_target_tenant(&org)
            .map_err(|error| DataError::internal("audit mutation", error))?;
        let key = self.audit_key()?;
        let event = AuditEvent {
            operation: operation.into(),
            target_type: "data_source".into(),
            target_id: id.into(),
            result: result.into(),
            classification: "admin".into(),
            revision,
        };
        interact(pool, move |c| {
            let tx = c
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| DataError::internal("audit mutation", error))?;
            append_in_transaction(
                &tx,
                &key,
                &audit
                    .event_id()
                    .map_err(|error| DataError::internal("audit mutation", error))?,
                audit.context(),
                &event,
            )
            .map_err(|error| DataError::internal("append managed source audit", error))?;
            tx.commit()
                .map_err(|error| DataError::internal("audit mutation", error))
        })
        .await
    }
    pub(super) async fn source_view(
        &self,
        source: SourceConfig,
        enabled: bool,
        version: u64,
        origin: &str,
    ) -> SourceView {
        let id = source.id.clone();
        let mut health = self
            .health
            .read()
            .await
            .get(&id)
            .cloned()
            .unwrap_or_else(|| {
                let mut h = SourceHealth::default();
                if !enabled {
                    h.state = "disabled".into();
                } else if !self.missing_references(&source).is_empty() {
                    h.state = "config_error".into();
                }
                h
            });
        if !enabled {
            health.state = "disabled".into();
        } else if !self.missing_references(&source).is_empty() {
            health.state = "config_error".into();
        }
        let artifact_count = self
            .impact(&id)
            .await
            .map(|v| v.len() as u64)
            .unwrap_or_default();
        SourceView {
            id,
            org: source.org.clone(),
            origin: origin.into(),
            enabled,
            version,
            missing_references: self.missing_references(&source),
            health,
            artifact_count,
            definition: source,
        }
    }
    pub async fn source_views(&self, org: Option<&str>) -> Result<Vec<SourceView>, DataError> {
        self.check_registry().await?;
        let mut views = Vec::new();
        for source in self
            .sources
            .values()
            .filter(|s| org.is_none_or(|o| o == s.org))
        {
            views.push(self.source_view(source.clone(), true, 1, "operator").await);
        }
        for (source, enabled, version) in self.managed_rows().await? {
            if org.is_none_or(|o| o == source.org) {
                views.push(self.source_view(source, enabled, version, "managed").await);
            }
        }
        views.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(views)
    }
    pub async fn source_view_by_id(&self, id: &str) -> Result<SourceView, DataError> {
        if let Some(source) = self.sources.get(id) {
            return Ok(self.source_view(source.clone(), true, 1, "operator").await);
        }
        let (source, enabled, version, origin) = self.source_record(id).await?;
        Ok(self.source_view(source, enabled, version, &origin).await)
    }
    pub async fn create_managed_source(
        &self,
        source: SourceConfig,
        enabled: bool,
        audit: MutationAudit,
    ) -> Result<SourceView, DataError> {
        let _guard = self.writes.lock().await;
        validate_source(&source)?;
        validate_managed_credentials(&source)?;
        if self.sources.contains_key(&source.id) {
            return Err(DataError::SourceConflict);
        }
        let Some(pool) = &self.pool else {
            return Err(DataError::Unavailable);
        };
        let id = source.id.clone();
        let org = source.org.clone();
        let audit = audit
            .for_target_tenant(&org)
            .map_err(|error| DataError::internal("create managed source", error))?;
        let raw = serde_json::to_string(&source).map_err(|_| DataError::BadParams)?;
        let audit_key = self.audit_key()?;
        interact(pool, move |c| {
            let tx = c
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| DataError::internal("create managed source", error))?;
            let exists: Option<i64> = tx
                .query_row(SOURCE_EXISTS, [&id], |r| r.get(0))
                .optional()
                .map_err(|error| DataError::internal("create managed source", error))?;
            if exists.is_some() {
                return Err(DataError::SourceConflict);
            }
            let org_exists: Option<i64> = tx
                .query_row(ORG_EXISTS, [&org], |r| r.get(0))
                .optional()
                .map_err(|error| DataError::internal("create managed source", error))?;
            if org_exists.is_none() {
                return Err(DataError::UnknownOrg);
            }
            tx.execute(
                INSERT_MANAGED_SOURCE,
                rusqlite::params![id, org, raw, enabled as i64],
            )
            .map_err(|error| DataError::internal("create managed source", error))?;
            let event = AuditEvent {
                operation: "data_source.create".into(),
                target_type: "data_source".into(),
                target_id: id.clone(),
                result: "success".into(),
                classification: "admin".into(),
                revision: Some(1),
            };
            append_in_transaction(
                &tx,
                &audit_key,
                &audit
                    .event_id()
                    .map_err(|error| DataError::internal("create managed source", error))?,
                audit.context(),
                &event,
            )
            .map_err(|error| DataError::internal("append managed source audit", error))?;
            tx.commit()
                .map_err(|error| DataError::internal("create managed source", error))
        })
        .await?;
        self.source_view_by_id(&source.id).await
    }
    pub async fn set_managed_source(
        &self,
        id: &str,
        source: SourceConfig,
        enabled: bool,
        expected: u64,
        audit: MutationAudit,
    ) -> Result<SourceView, DataError> {
        let _guard = self.writes.lock().await;
        validate_source(&source)?;
        validate_managed_credentials(&source)?;
        if id != source.id {
            return Err(DataError::InvalidSource);
        }
        if self.sources.contains_key(id) {
            return Err(DataError::OperatorReadOnly);
        }
        let Some(pool) = &self.pool else {
            return Err(DataError::Unavailable);
        };
        let raw = serde_json::to_string(&source).map_err(|_| DataError::BadParams)?;
        let id_owned = id.to_owned();
        let org = source.org.clone();
        let audit = audit
            .for_target_tenant(&org)
            .map_err(|error| DataError::internal("set managed source", error))?;
        let audit_key = self.audit_key()?;
        interact(pool, move |c| {
            let tx = c
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| DataError::internal("set managed source", error))?;
            let row: Option<(String, i64, i64)> = tx
                .query_row(SELECT_MANAGED_SOURCE_VERSION, [&id_owned], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()
                .map_err(|error| DataError::internal("set managed source", error))?;
            let Some((old_org, version, _)) = row else {
                return Err(DataError::NotFound);
            };
            let version = u64::try_from(version)
                .map_err(|error| DataError::internal("set managed source", error))?;
            if version != expected {
                return Err(DataError::StaleVersion);
            }
            let org_exists: Option<i64> = tx
                .query_row(ORG_EXISTS, [&org], |r| r.get(0))
                .optional()
                .map_err(|error| DataError::internal("set managed source", error))?;
            if org_exists.is_none() {
                return Err(DataError::UnknownOrg);
            }
            if old_org != org {
                let count: i64 = bound_source_count(&tx, &id_owned)
                    .map_err(|error| DataError::internal("set managed source", error))?;
                if count > 0 {
                    return Err(DataError::OrgInUse);
                }
            }
            let changed: i64 = bound_capability_count(&tx, &id_owned, &raw)
                .map_err(|error| DataError::internal("set managed source", error))?;
            if changed > 0 {
                return Err(DataError::BindingInUse);
            }
            tx.execute(
                UPDATE_MANAGED_SOURCE,
                rusqlite::params![id_owned, org, raw, enabled as i64],
            )
            .map_err(|error| DataError::internal("set managed source", error))?;
            let event = AuditEvent {
                operation: "data_source.update".into(),
                target_type: "data_source".into(),
                target_id: id_owned.clone(),
                result: "success".into(),
                classification: "admin".into(),
                revision: Some(version + 1),
            };
            append_in_transaction(
                &tx,
                &audit_key,
                &audit
                    .event_id()
                    .map_err(|error| DataError::internal("set managed source", error))?,
                audit.context(),
                &event,
            )
            .map_err(|error| DataError::internal("append managed source audit", error))?;
            tx.commit()
                .map_err(|error| DataError::internal("set managed source", error))
        })
        .await?;
        self.source_view_by_id(id).await
    }
    pub async fn set_managed_enabled(
        &self,
        id: &str,
        enabled: bool,
        expected: u64,
        audit: MutationAudit,
    ) -> Result<SourceView, DataError> {
        let _guard = self.writes.lock().await;
        if self.sources.contains_key(id) {
            return Err(DataError::OperatorReadOnly);
        }
        let Some(pool) = &self.pool else {
            return Err(DataError::Unavailable);
        };
        let id_owned = id.to_owned();
        let org = self.source_record(id).await?.0.org;
        let audit = audit
            .for_target_tenant(&org)
            .map_err(|error| DataError::internal("set managed enabled", error))?;
        let audit_key = self.audit_key()?;
        interact(pool, move |c| {
            let tx = c
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| DataError::internal("set managed enabled", error))?;
            let changed = tx
                .execute(
                    UPDATE_MANAGED_ENABLED,
                    rusqlite::params![id_owned, enabled as i64, expected as i64],
                )
                .map_err(|error| DataError::internal("set managed enabled", error))?;
            if changed == 0 {
                return Err(DataError::StaleVersion);
            }
            let event = AuditEvent {
                operation: if enabled {
                    "data_source.enable"
                } else {
                    "data_source.disable"
                }
                .into(),
                target_type: "data_source".into(),
                target_id: id_owned.clone(),
                result: "success".into(),
                classification: "admin".into(),
                revision: Some(expected + 1),
            };
            append_in_transaction(
                &tx,
                &audit_key,
                &audit
                    .event_id()
                    .map_err(|error| DataError::internal("set managed enabled", error))?,
                audit.context(),
                &event,
            )
            .map_err(|error| DataError::internal("append managed source audit", error))?;
            tx.commit()
                .map_err(|error| DataError::internal("set managed enabled", error))
        })
        .await?;
        self.source_view_by_id(id).await
    }
    pub async fn delete_managed(
        &self,
        id: &str,
        expected: u64,
        audit: MutationAudit,
    ) -> Result<(), DataError> {
        let _guard = self.writes.lock().await;
        if self.sources.contains_key(id) {
            return Err(DataError::OperatorReadOnly);
        }
        let Some(pool) = &self.pool else {
            return Err(DataError::Unavailable);
        };
        let id_owned = id.to_owned();
        let org = self.source_record(id).await?.0.org;
        let audit = audit
            .for_target_tenant(&org)
            .map_err(|error| DataError::internal("delete managed", error))?;
        let audit_key = self.audit_key()?;
        interact(pool, move |c| {
            let tx = c
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| DataError::internal("delete managed", error))?;
            let row: Option<i64> = tx
                .query_row(SELECT_SOURCE_VERSION, [&id_owned], |r| r.get(0))
                .optional()
                .map_err(|error| DataError::internal("delete managed", error))?;
            let Some(version) = row else {
                return Err(DataError::NotFound);
            };
            let version = u64::try_from(version)
                .map_err(|error| DataError::internal("delete managed", error))?;
            if version != expected {
                return Err(DataError::StaleVersion);
            }
            if bound_source_count(&tx, &id_owned)
                .map_err(|error| DataError::internal("delete managed", error))?
                > 0
            {
                return Err(DataError::SourceInUse);
            }
            tx.execute(DELETE_MANAGED_SOURCE, [&id_owned])
                .map_err(|error| DataError::internal("delete managed", error))?;
            let event = AuditEvent {
                operation: "data_source.delete".into(),
                target_type: "data_source".into(),
                target_id: id_owned.clone(),
                result: "success".into(),
                classification: "admin".into(),
                revision: Some(expected),
            };
            append_in_transaction(
                &tx,
                &audit_key,
                &audit
                    .event_id()
                    .map_err(|error| DataError::internal("delete managed", error))?,
                audit.context(),
                &event,
            )
            .map_err(|error| DataError::internal("append managed source audit", error))?;
            tx.commit()
                .map_err(|error| DataError::internal("delete managed", error))
        })
        .await
    }
    pub async fn impact(&self, source_id: &str) -> Result<Vec<ImpactArtifact>, DataError> {
        let Some(pool) = &self.pool else {
            return Ok(Vec::new());
        };
        let source = source_id.to_owned();
        interact(pool, move |connection| {
            let mut statement = connection.prepare(SELECT_BOUND_ARTIFACTS).map_err(|error| DataError::internal("impact", error))?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?, row.get::<_, String>(3)?))
            }).map_err(|error| DataError::internal("impact", error))?;
            let mut artifacts = Vec::new();
            for row in rows {
                let (id, title, org, raw) = row.map_err(|error| DataError::internal("impact", error))?;
                let manifest: BindingManifest = serde_json::from_str(&raw).map_err(|error| DataError::internal("impact", error))?;
                let bindings = manifest.bindings.into_iter()
                    .filter(|(_, binding)| binding.source == source)
                    .map(|(name, binding)| json!({"name":name,"operations":binding.operations,"subscriptions":binding.subscriptions}))
                    .collect::<Vec<_>>();
                if !bindings.is_empty() {
                    artifacts.push(ImpactArtifact { id: id.clone(), title, org, url: format!("/{id}"), bindings });
                }
            }
            Ok(artifacts)
        }).await
    }
    pub async fn test_source(
        &self,
        id: &str,
        operation: &str,
        params: Value,
    ) -> Result<TestResult, DataError> {
        let (source, enabled, _, _) = self.source_record(id).await?;
        if !enabled {
            return Err(DataError::TestUnavailable);
        }
        if !self.missing_references(&source).is_empty() {
            return Err(DataError::TestUnavailable);
        }
        if source.kind == "push" {
            if !operation.is_empty() {
                return Err(DataError::BadParams);
            }
            if !params.as_object().is_some_and(|v| v.is_empty()) {
                return Err(DataError::BadParams);
            }
            self.record_query_health(id, Ok(())).await;
            return Ok(TestResult {
                ok: true,
                source_id: id.into(),
                elapsed_ms: 0,
                summary: json!({"kind":"push"}),
                error: None,
            });
        }
        if !source.operations.contains_key(operation) {
            return Err(DataError::BadParams);
        }
        let op = source
            .operations
            .get(operation)
            .ok_or(DataError::NotFound)?
            .clone();
        build_url(&source, &op, &params)?;
        let started = std::time::Instant::now();
        let result = self.http_query(&source, &op, &params).await;
        self.record_query_health(&source.id, result.as_ref().map(|_| ()))
            .await;
        let elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        match result {
            Ok(value) => Ok(TestResult {
                ok: true,
                source_id: id.into(),
                elapsed_ms,
                summary: json!({"kind":"http","response_type":if value.is_object(){"object"}else if value.is_array(){"array"}else{"scalar"},"bytes":serde_json::to_vec(&value).map(|v|v.len()).unwrap_or(0)}),
                error: None,
            }),
            Err(error) => Ok(TestResult {
                ok: false,
                source_id: id.into(),
                elapsed_ms,
                summary: json!({"kind":"http"}),
                error: Some(error.query_reason().into()),
            }),
        }
    }
}
pub(super) fn bound_source_count(
    tx: &rusqlite::Transaction<'_>,
    source: &str,
) -> rusqlite::Result<i64> {
    let mut stmt = tx.prepare(SELECT_CURRENT_BINDINGS)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut count = 0;
    for row in rows {
        let raw = row?;
        if serde_json::from_str::<BindingManifest>(&raw)
            .ok()
            .is_some_and(|m| m.bindings.values().any(|b| b.source == source))
        {
            count += 1;
        }
    }
    Ok(count)
}
pub(super) fn bound_capability_count(
    tx: &rusqlite::Transaction<'_>,
    source: &str,
    next_raw: &str,
) -> rusqlite::Result<i64> {
    let next: SourceConfig =
        serde_json::from_str(next_raw).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let mut stmt = tx.prepare(SELECT_CURRENT_BINDINGS)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut count = 0;
    for row in rows {
        let raw = row?;
        let Ok(manifest) = serde_json::from_str::<BindingManifest>(&raw) else {
            continue;
        };
        for binding in manifest.bindings.values().filter(|b| b.source == source) {
            if binding
                .operations
                .iter()
                .any(|n| !next.operations.contains_key(n))
                || binding
                    .subscriptions
                    .iter()
                    .any(|n| !next.subscriptions.contains_key(n))
            {
                count += 1;
            }
        }
    }
    Ok(count)
}
