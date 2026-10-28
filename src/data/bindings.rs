//! Per-artifact binding manifests.
use super::{Binding, BindingManifest, DataBroker, SourceConfig, validate::validate_bindings};
use crate::{
    error::AppError,
    persistence::db::interact,
    security::audit::{AuditEvent, MutationAudit, append_in_transaction},
};
use rusqlite::{OptionalExtension, TransactionBehavior};

impl DataBroker {
    pub(super) async fn load_bindings(
        &self,
        id: &str,
    ) -> Result<(String, BindingManifest), String> {
        if let Some(pool) = &self.pool {
            let id = id.to_owned();
            let row=interact(pool,move|c| c.query_row("SELECT b.org,b.bindings,a.org FROM artifact_data_bindings b JOIN artifacts a ON a.id=b.artifact_id WHERE b.artifact_id=?1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).optional().map_err(|_|AppError::Internal)).await.map_err(|_|"data_unavailable")?;
            let Some((org, raw, current)) = row else {
                return Ok((String::new(), BindingManifest::default()));
            };
            if org != current {
                return Err("not_found".into());
            }
            let manifest: BindingManifest =
                serde_json::from_str(&raw).map_err(|_| "data_unavailable")?;
            return Ok((org, manifest));
        }
        Ok(self
            .memory
            .read()
            .await
            .bindings
            .get(id)
            .cloned()
            .unwrap_or_default())
    }
    pub async fn try_bindings(&self, id: &str) -> Result<BindingManifest, String> {
        let (org, m) = self.load_bindings(id).await?;
        let sources = self.all_source_configs().await?;
        validate_bindings(&m, &sources, &org).map_err(|_| "not_found")?;
        Ok(m)
    }
    pub async fn get_bindings(&self, id: &str) -> BindingManifest {
        self.try_bindings(id).await.unwrap_or_default()
    }
    pub async fn set_bindings(
        &self,
        id: &str,
        manifest: BindingManifest,
        org: &str,
    ) -> Result<(), String> {
        self.set_bindings_inner(id, manifest, org, None).await
    }
    pub async fn set_bindings_audited(
        &self,
        id: &str,
        manifest: BindingManifest,
        org: &str,
        audit: MutationAudit,
    ) -> Result<(), String> {
        let audit = audit
            .for_target_tenant(org)
            .map_err(|_| "data_unavailable".to_owned())?;
        self.set_bindings_inner(id, manifest, org, Some(audit))
            .await
    }
    pub(super) async fn set_bindings_inner(
        &self,
        id: &str,
        manifest: BindingManifest,
        org: &str,
        audit: Option<MutationAudit>,
    ) -> Result<(), String> {
        let _guard = self.writes.lock().await;
        let sources = self.all_source_configs().await?;
        validate_bindings(&manifest, &sources, org)?;
        if let Some(pool) = &self.pool {
            let id = id.to_owned();
            let org = org.to_owned();
            let raw = serde_json::to_string(&manifest).map_err(|_| "bad_params")?;
            let audit_key = if audit.is_some() {
                Some(self.audit_key()?)
            } else {
                None
            };
            interact(pool,move|c|{
                // Reserve the writer before reading so other WAL writers cannot stale our snapshot.
                let t=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_|AppError::Internal)?;
                let previous=t.query_row("SELECT org,bindings FROM artifact_data_bindings WHERE artifact_id=?1",[&id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional().map_err(|_|AppError::Internal)?;
                let mut removed=Vec::new();
                if let Some((previous_org,previous_raw))=previous {
                    let old:BindingManifest=serde_json::from_str(&previous_raw).map_err(|_|AppError::Internal)?;
                    let next:BindingManifest=serde_json::from_str(&raw).map_err(|_|AppError::Internal)?;
                    removed=old.bindings.iter().filter(|(name,b)|previous_org!=org || next.bindings.get(*name)!=Some(*b)).map(|(n,_)|n.clone()).collect();
                }
                for binding in removed {
                    t.execute("DELETE FROM artifact_data_snapshots WHERE artifact_id=?1 AND binding=?2",(&id,&binding)).map_err(|_|AppError::Internal)?;
                    t.execute("DELETE FROM artifact_data_events WHERE artifact_id=?1 AND binding=?2",(&id,&binding)).map_err(|_|AppError::Internal)?;
                }
                t.execute("INSERT INTO artifact_data_bindings(artifact_id,org,bindings,updated_at) VALUES(?1,?2,?3,datetime('now')) ON CONFLICT(artifact_id) DO UPDATE SET org=excluded.org,bindings=excluded.bindings,updated_at=excluded.updated_at",(&id,&org,&raw)).map_err(|_|AppError::Internal)?;
                if let (Some(audit), Some(key)) = (audit, audit_key) {
                    let event = AuditEvent { operation: "data_bindings.update".into(), target_type: "artifact".into(), target_id: id.clone(), result: "success".into(), classification: "admin".into(), revision: None };
                    append_in_transaction(&t, &key, &audit.event_id().map_err(|_| AppError::Internal)?, audit.context(), &event)?;
                }
                t.commit().map_err(|_|AppError::Internal)
            }).await.map_err(|_|"data_unavailable")?;
        } else {
            let mut memory = self.memory.write().await;
            let (old_org, old) = memory.bindings.get(id).cloned().unwrap_or_default();
            let removed: Vec<String> = old
                .bindings
                .iter()
                .filter(|(n, b)| old_org != org || manifest.bindings.get(*n) != Some(*b))
                .map(|(n, _)| n.clone())
                .collect();

            memory
                .snapshots
                .retain(|(a, b, _), _| a != id || !removed.contains(b));
            memory
                .events
                .retain(|(a, b, _), _| a != id || !removed.contains(b));
            memory.bindings.insert(id.into(), (org.into(), manifest));
        }
        Ok(())
    }
    pub(super) async fn binding_source(
        &self,
        id: &str,
        binding: &str,
    ) -> Result<(Binding, SourceConfig), String> {
        let (org, manifest) = self.load_bindings(id).await?;
        let b = manifest.bindings.get(binding).ok_or("not_found")?.clone();
        let (s, enabled, _, _) = self.source_record(&b.source).await?;
        if s.org != org {
            return Err("not_found".into());
        }
        if !enabled {
            return Err("data_unavailable".into());
        }
        Ok((b, s))
    }
}
