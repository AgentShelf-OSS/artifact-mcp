//! Push-source snapshot and event store.
use super::{DataBroker, Event, MAX_BYTES, MAX_EVENTS, validate::valid_event_text};
use crate::{error::AppError, persistence::db::interact};
use serde_json::Value;

impl DataBroker {
    pub async fn set_snapshot(
        &self,
        id: &str,
        binding: &str,
        key: &str,
        value: Value,
    ) -> Result<u64, String> {
        let raw = serde_json::to_string(&value).map_err(|_| "bad_params")?;
        if raw.len() > 256 * 1024 {
            return Err("too_large".into());
        }
        let _guard = self.writes.lock().await;
        let (b, s) = self.binding_source(id, binding).await?;
        if s.kind != "push"
            || !b.operations.iter().any(|n| {
                s.operations
                    .get(n)
                    .is_some_and(|o| o.key.as_deref().unwrap_or(n) == key)
            })
        {
            return Err("not_found".into());
        }
        if let Some(pool) = &self.pool {
            let a = id.to_owned();
            let b = binding.to_owned();
            let k = key.to_owned();
            return interact(pool,move|c|{
                c.query_row("INSERT INTO artifact_data_snapshots(artifact_id,binding,key,value,revision,updated_at) VALUES(?1,?2,?3,?4,1,datetime('now')) ON CONFLICT(artifact_id,binding,key) DO UPDATE SET value=excluded.value,revision=revision+1,updated_at=excluded.updated_at RETURNING revision",(&a,&b,&k,&raw),|r|r.get::<_,i64>(0).map(|v|v as u64)).map_err(|_|AppError::Internal)
            }).await.map_err(|_|"data_unavailable".into());
        }
        let mut memory = self.memory.write().await;
        let k = (id.into(), binding.into(), key.into());
        let revision = memory.snapshots.get(&k).map_or(1, |(r, _)| r + 1);
        memory.snapshots.insert(k, (revision, value));
        Ok(revision)
    }
    pub async fn append_events(
        &self,
        id: &str,
        binding: &str,
        sub: &str,
        items: Vec<Event>,
    ) -> Result<(usize, usize), String> {
        if items.len() > 100
            || items
                .iter()
                .any(|e| !valid_event_text(&e.id) || !valid_event_text(&e.event))
        {
            return Err("bad_params".into());
        }
        if serde_json::to_vec(&items).map_err(|_| "bad_params")?.len() > MAX_BYTES {
            return Err("too_large".into());
        }
        let _guard = self.writes.lock().await;
        let (b, s) = self.binding_source(id, binding).await?;
        if s.kind != "push" || !b.subscriptions.iter().any(|n| n == sub) {
            return Err("not_found".into());
        }
        if let Some(pool) = &self.pool {
            let a = id.to_owned();
            let b = binding.to_owned();
            let sub = sub.to_owned();
            return interact(pool,move|c|{
                let t=c.transaction().map_err(|_|AppError::Internal)?;let mut accepted=0;
                for e in &items {
                    let raw=serde_json::to_string(&e.data).map_err(|_|AppError::Internal)?;
                    accepted+=t.execute("INSERT OR IGNORE INTO artifact_data_events(artifact_id,binding,subscription,event_id,event_name,data,created_at) VALUES(?1,?2,?3,?4,?5,?6,datetime('now'))",(&a,&b,&sub,&e.id,&e.event,&raw)).map_err(|_|AppError::Internal)?;
                }
                t.execute("DELETE FROM artifact_data_events WHERE artifact_id=?1 AND binding=?2 AND subscription=?3 AND rowid NOT IN(SELECT rowid FROM artifact_data_events WHERE artifact_id=?1 AND binding=?2 AND subscription=?3 ORDER BY rowid DESC LIMIT 1000)",(&a,&b,&sub)).map_err(|_|AppError::Internal)?;
                t.commit().map_err(|_|AppError::Internal)?;Ok((accepted,items.len()-accepted))
            }).await.map_err(|_|"data_unavailable".into());
        }
        let mut memory = self.memory.write().await;
        let list = memory
            .events
            .entry((id.into(), binding.into(), sub.into()))
            .or_default();
        let mut accepted = 0;
        for e in &items {
            if !list.iter().any(|x| x.id == e.id) {
                list.push(e.clone());
                accepted += 1;
            }
        }
        if list.len() > MAX_EVENTS {
            list.drain(..list.len() - MAX_EVENTS);
        }
        Ok((accepted, items.len() - accepted))
    }
    pub(super) async fn stored_events(
        &self,
        id: &str,
        binding: &str,
        sub: &str,
    ) -> Result<Vec<Event>, String> {
        if let Some(pool) = &self.pool {
            let a = id.to_owned();
            let b = binding.to_owned();
            let s = sub.to_owned();
            return interact(pool,move|c|{
                let mut stmt=c.prepare("SELECT event_id,event_name,data FROM artifact_data_events WHERE artifact_id=?1 AND binding=?2 AND subscription=?3 ORDER BY rowid").map_err(|_|AppError::Internal)?;
                let rows=stmt.query_map((&a,&b,&s),|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).map_err(|_|AppError::Internal)?;
                let mut out=Vec::new();for row in rows { let (id,event,raw)=row.map_err(|_|AppError::Internal)?;out.push(Event{id,event,data:serde_json::from_str(&raw).map_err(|_|AppError::Internal)?}); } Ok(out)
            }).await.map_err(|_|"data_unavailable".into());
        }
        Ok(self
            .memory
            .read()
            .await
            .events
            .get(&(id.into(), binding.into(), sub.into()))
            .cloned()
            .unwrap_or_default())
    }
}
