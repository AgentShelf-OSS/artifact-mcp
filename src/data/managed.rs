//! Managed-source administration: views, CRUD with audit, impact, and connection tests.
use super::{
    BindingManifest, DataBroker, ImpactArtifact, SourceConfig, SourceHealth, SourceView,
    TestResult,
    validate::{build_url, validate_managed_credentials, validate_source},
};
use crate::{
    error::AppError,
    persistence::db::interact,
    security::audit::{AuditEvent, MutationAudit, append_in_transaction},
};
use rusqlite::{OptionalExtension, TransactionBehavior};
use serde_json::{Value, json};

impl DataBroker {
    pub async fn audit_mutation(
        &self,
        audit: MutationAudit,
        operation: &str,
        id: &str,
        _org: &str,
        result: &str,
        revision: Option<u64>,
    ) -> Result<(), String> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };
        let org = self.source_record(id).await?.0.org;
        let audit = audit
            .for_target_tenant(&org)
            .map_err(|_| "data_unavailable".to_owned())?;
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
                .map_err(|_| AppError::Internal)?;
            append_in_transaction(
                &tx,
                &key,
                &audit.event_id().map_err(|_| AppError::Internal)?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)
        })
        .await
        .map_err(|_| "data_unavailable".into())
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
    pub async fn source_views(&self, org: Option<&str>) -> Result<Vec<SourceView>, String> {
        self.validate_registry().await?;
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
    pub async fn source_view_by_id(&self, id: &str) -> Result<SourceView, String> {
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
    ) -> Result<SourceView, String> {
        let _guard = self.writes.lock().await;
        validate_source(&source)?;
        validate_managed_credentials(&source)?;
        if self.sources.contains_key(&source.id) {
            return Err("source_conflict".into());
        }
        let Some(pool) = &self.pool else {
            return Err("data_unavailable".into());
        };
        let id = source.id.clone();
        let org = source.org.clone();
        let audit = audit
            .for_target_tenant(&org)
            .map_err(|_| "data_unavailable".to_owned())?;
        let raw = serde_json::to_string(&source).map_err(|_| "bad_params")?;
        let audit_key = self.audit_key()?;
        interact(pool, move |c| {
            let tx = c
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let exists: Option<i64> = tx
                .query_row("SELECT 1 FROM data_sources WHERE id=?1", [&id], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(|_| AppError::Internal)?;
            if exists.is_some() {
                return Err(AppError::Conflict("source_conflict".into()));
            }
            let org_exists: Option<i64> = tx
                .query_row("SELECT 1 FROM orgs WHERE name=?1", [&org], |r| r.get(0))
                .optional()
                .map_err(|_| AppError::Internal)?;
            if org_exists.is_none() {
                return Err(AppError::Validation("unknown_org".into()));
            }
            tx.execute(
                "INSERT INTO data_sources(id,org,definition,enabled,version) VALUES(?1,?2,?3,?4,1)",
                rusqlite::params![id, org, raw, enabled as i64],
            )
            .map_err(|_| AppError::Internal)?;
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
                &audit.event_id().map_err(|_| AppError::Internal)?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)
        })
        .await
        .map_err(|e| match e {
            AppError::Conflict(_) => String::from("source_conflict"),
            AppError::Validation(_) => String::from("unknown_org"),
            _ => String::from("data_unavailable"),
        })?;
        self.source_view_by_id(&source.id).await
    }
    pub async fn set_managed_source(
        &self,
        id: &str,
        source: SourceConfig,
        enabled: bool,
        expected: u64,
        audit: MutationAudit,
    ) -> Result<SourceView, String> {
        let _guard = self.writes.lock().await;
        validate_source(&source)?;
        validate_managed_credentials(&source)?;
        if id != source.id {
            return Err("invalid_source".into());
        }
        if self.sources.contains_key(id) {
            return Err("operator_read_only".into());
        }
        let Some(pool) = &self.pool else {
            return Err("data_unavailable".into());
        };
        let raw = serde_json::to_string(&source).map_err(|_| "bad_params")?;
        let id_owned = id.to_owned();
        let org = source.org.clone();
        let audit = audit
            .for_target_tenant(&org)
            .map_err(|_| "data_unavailable".to_owned())?;
        let audit_key = self.audit_key()?;
        interact(pool, move |c| {
            let tx=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_| AppError::Internal)?;
            let row: Option<(String,i64,i64)> = tx.query_row("SELECT org,version,enabled FROM data_sources WHERE id=?1", [&id_owned], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|_| AppError::Internal)?;
            let Some((old_org,version,_))=row else { return Err(AppError::NotFound("not_found".into())); };
            if version as u64 != expected { return Err(AppError::Conflict("stale_version".into())); }
            let org_exists: Option<i64> = tx.query_row("SELECT 1 FROM orgs WHERE name=?1", [&org], |r| r.get(0)).optional().map_err(|_| AppError::Internal)?;
            if org_exists.is_none() { return Err(AppError::Validation("unknown_org".into())); }
            if old_org != org { let count: i64=bound_source_count(&tx,&id_owned).map_err(|_| AppError::Internal)?; if count>0 { return Err(AppError::Conflict("org_in_use".into())); } }
            let changed: i64=bound_capability_count(&tx,&id_owned,&raw).map_err(|_| AppError::Internal)?; if changed>0 { return Err(AppError::Conflict("binding_in_use".into())); }
            tx.execute("UPDATE data_sources SET org=?2,definition=?3,enabled=?4,version=version+1,updated_at=datetime('now') WHERE id=?1",rusqlite::params![id_owned,org,raw,enabled as i64]).map_err(|_|AppError::Internal)?;
            let event = AuditEvent { operation: "data_source.update".into(), target_type: "data_source".into(), target_id: id_owned.clone(), result: "success".into(), classification: "admin".into(), revision: Some((version + 1) as u64) };
            append_in_transaction(&tx, &audit_key, &audit.event_id().map_err(|_| AppError::Internal)?, audit.context(), &event)?;
            tx.commit().map_err(|_|AppError::Internal)
        }).await.map_err(|e| match e { AppError::Conflict(x)=>x,AppError::NotFound(_)=>"not_found".into(),AppError::Validation(x)=>x,_=>"data_unavailable".into() })?;
        self.source_view_by_id(id).await
    }
    pub async fn set_managed_enabled(
        &self,
        id: &str,
        enabled: bool,
        expected: u64,
        audit: MutationAudit,
    ) -> Result<SourceView, String> {
        let _guard = self.writes.lock().await;
        if self.sources.contains_key(id) {
            return Err("operator_read_only".into());
        }
        let Some(pool) = &self.pool else {
            return Err("data_unavailable".into());
        };
        let id_owned = id.to_owned();
        let org = self
            .source_record(id)
            .await
            .map_err(|e| e.to_owned())?
            .0
            .org;
        let audit = audit
            .for_target_tenant(&org)
            .map_err(|_| "data_unavailable".to_owned())?;
        let audit_key = self.audit_key()?;
        interact(pool, move |c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_| AppError::Internal)?;
            let changed = tx.execute("UPDATE data_sources SET enabled=?2,version=version+1,updated_at=datetime('now') WHERE id=?1 AND version=?3", rusqlite::params![id_owned, enabled as i64, expected as i64]).map_err(|_| AppError::Internal)?;
            if changed == 0 { return Err(AppError::Conflict("stale_version".into())); }
            let event = AuditEvent { operation: if enabled { "data_source.enable" } else { "data_source.disable" }.into(), target_type: "data_source".into(), target_id: id_owned.clone(), result: "success".into(), classification: "admin".into(), revision: Some(expected + 1) };
            append_in_transaction(&tx, &audit_key, &audit.event_id().map_err(|_| AppError::Internal)?, audit.context(), &event)?;
            tx.commit().map_err(|_| AppError::Internal)
        }).await.map_err(|e| match e { AppError::Conflict(_) => String::from("stale_version"), _ => String::from("data_unavailable") })?;
        self.source_view_by_id(id).await
    }
    pub async fn delete_managed(
        &self,
        id: &str,
        expected: u64,
        audit: MutationAudit,
    ) -> Result<(), String> {
        let _guard = self.writes.lock().await;
        if self.sources.contains_key(id) {
            return Err("operator_read_only".into());
        }
        let Some(pool) = &self.pool else {
            return Err("data_unavailable".into());
        };
        let id_owned = id.to_owned();
        let org = self
            .source_record(id)
            .await
            .map_err(|e| e.to_owned())?
            .0
            .org;
        let audit = audit
            .for_target_tenant(&org)
            .map_err(|_| "data_unavailable".to_owned())?;
        let audit_key = self.audit_key()?;
        interact(pool, move |c| {
            let tx = c
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let row: Option<i64> = tx
                .query_row(
                    "SELECT version FROM data_sources WHERE id=?1",
                    [&id_owned],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|_| AppError::Internal)?;
            let Some(version) = row else {
                return Err(AppError::NotFound("not_found".into()));
            };
            if version as u64 != expected {
                return Err(AppError::Conflict("stale_version".into()));
            }
            if bound_source_count(&tx, &id_owned).map_err(|_| AppError::Internal)? > 0 {
                return Err(AppError::Conflict("source_in_use".into()));
            }
            tx.execute("DELETE FROM data_sources WHERE id=?1", [&id_owned])
                .map_err(|_| AppError::Internal)?;
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
                &audit.event_id().map_err(|_| AppError::Internal)?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)
        })
        .await
        .map_err(|e| match e {
            AppError::Conflict(x) => x,
            AppError::NotFound(_) => "not_found".into(),
            _ => "data_unavailable".into(),
        })
    }
    pub async fn impact(&self, source_id: &str) -> Result<Vec<ImpactArtifact>, String> {
        let Some(pool) = &self.pool else {
            return Ok(Vec::new());
        };
        let source = source_id.to_owned();
        interact(pool,move|c|{let mut stmt=c.prepare("SELECT a.id,a.title,a.org,b.bindings FROM artifact_data_bindings b JOIN artifacts a ON a.id=b.artifact_id WHERE a.org=b.org").map_err(|_|AppError::Internal)?;let rows=stmt.query_map([],|r|{let raw:String=r.get(3)?;Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,raw))}).map_err(|_|AppError::Internal)?;let mut out=Vec::new();for row in rows{let(id,title,org,raw)=row.map_err(|_|AppError::Internal)?;let m:BindingManifest=serde_json::from_str(&raw).map_err(|_|AppError::Internal)?;let bindings=m.bindings.into_iter().filter(|(_,b)|b.source==source).map(|(name,b)|json!({"name":name,"operations":b.operations,"subscriptions":b.subscriptions})).collect::<Vec<_>>();if !bindings.is_empty(){out.push(ImpactArtifact{id:id.clone(),title,org,url:format!("/{id}"),bindings});}}Ok(out)}).await.map_err(|_|"data_unavailable".into())
    }
    pub async fn test_source(
        &self,
        id: &str,
        operation: &str,
        params: Value,
    ) -> Result<TestResult, String> {
        let (source, enabled, _, _) = self.source_record(id).await?;
        if !enabled {
            return Err("test_unavailable".into());
        }
        if !self.missing_references(&source).is_empty() {
            return Err("test_unavailable".into());
        }
        if source.kind == "push" {
            if !operation.is_empty() {
                return Err("bad_params".into());
            }
            if !params.as_object().is_some_and(|v| v.is_empty()) {
                return Err("bad_params".into());
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
            return Err("bad_params".into());
        }
        let op = source.operations.get(operation).ok_or("not_found")?.clone();
        build_url(&source, &op, &params)?;
        let started = std::time::Instant::now();
        let result = self.http_query(&source, &op, &params).await;
        self.record_query_health(
            &source.id,
            result.as_ref().map(|_| ()).map_err(|e| e.as_str()),
        )
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
                error: Some(
                    match error.as_str() {
                        "too_large" => "too_large",
                        "bad_params" => "bad_params",
                        "not_found" => "not_found",
                        _ => "data_unavailable",
                    }
                    .into(),
                ),
            }),
        }
    }
}
pub(super) fn bound_source_count(
    tx: &rusqlite::Transaction<'_>,
    source: &str,
) -> rusqlite::Result<i64> {
    let mut stmt = tx.prepare("SELECT bindings FROM artifact_data_bindings b JOIN artifacts a ON a.id=b.artifact_id WHERE a.org=b.org")?;
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
    let mut stmt = tx.prepare("SELECT bindings FROM artifact_data_bindings b JOIN artifacts a ON a.id=b.artifact_id WHERE a.org=b.org")?;
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
