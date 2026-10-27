//! Source registry loading, lookup, and the public source catalog.
use super::{
    DataBroker, MAX_SOURCES, ParamConfig, PublicSource, SourceConfig, validate::validate_source,
};
use crate::{error::AppError, persistence::db::interact};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

impl DataBroker {
    pub fn from_env() -> Result<Self, String> {
        let Some(path) = std::env::var_os("ARTIFACT_DATA_SOURCES_FILE").filter(|p| !p.is_empty())
        else {
            return Ok(Self::empty());
        };
        let raw = std::fs::read_to_string(path).map_err(|_| "invalid_data_sources")?;
        Self::from_json(&raw)
    }
    pub fn from_json(raw: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Config {
            sources: Vec<SourceConfig>,
        }
        let config: Config = serde_json::from_str(raw).map_err(|_| "invalid_data_sources")?;
        if config.sources.len() > MAX_SOURCES {
            return Err("invalid_data_sources".into());
        }
        let mut broker = Self::empty();
        let mut sources = BTreeMap::new();
        let mut headers = BTreeMap::new();
        for source in config.sources {
            validate_source(&source)?;
            if sources.contains_key(&source.id) {
                return Err("invalid_data_sources".into());
            }
            let mut resolved = HeaderMap::new();
            for (name, env) in &source.headers_env {
                let name =
                    HeaderName::from_bytes(name.as_bytes()).map_err(|_| "invalid_data_sources")?;
                if matches!(
                    name.as_str(),
                    "host" | "cookie" | "connection" | "content-length" | "transfer-encoding"
                ) {
                    return Err("invalid_data_sources".into());
                }
                let value = std::env::var(env).map_err(|_| "invalid_data_sources")?;
                let mut value =
                    HeaderValue::from_str(&value).map_err(|_| "invalid_data_sources")?;
                value.set_sensitive(true);
                resolved.insert(name, value);
            }
            headers.insert(source.id.clone(), resolved);
            sources.insert(source.id.clone(), source);
        }
        broker.sources = Arc::new(sources);
        broker.headers = Arc::new(headers);
        Ok(broker)
    }
    pub(super) async fn managed_rows(&self) -> Result<Vec<(SourceConfig, bool, u64)>, String> {
        let Some(pool) = &self.pool else {
            return Ok(Vec::new());
        };
        let rows = interact(pool, |c| {
            let mut stmt = c
                .prepare("SELECT definition,enabled,version FROM data_sources ORDER BY id")
                .map_err(|_| AppError::Internal)?;
            let mapped = stmt
                .query_map([], |row| {
                    let raw: String = row.get(0)?;
                    let definition =
                        serde_json::from_str(&raw).map_err(|_| rusqlite::Error::InvalidQuery)?;
                    Ok((
                        definition,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, i64>(2)? as u64,
                    ))
                })
                .map_err(|_| AppError::Internal)?;
            mapped
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| AppError::Internal)
        })
        .await
        .map_err(|_| "data_unavailable".to_owned())?;
        Ok(rows)
    }
    pub(super) async fn source_record(
        &self,
        id: &str,
    ) -> Result<(SourceConfig, bool, u64, String), String> {
        if let Some(source) = self.sources.get(id) {
            return Ok((source.clone(), true, 0, "operator".into()));
        }
        let Some(pool) = &self.pool else {
            return Err("not_found".into());
        };
        let id_owned = id.to_owned();
        interact(pool, move |c| {
            c.query_row(
                "SELECT definition,enabled,version FROM data_sources WHERE id=?1",
                [&id_owned],
                |row| {
                    let raw: String = row.get(0)?;
                    let source =
                        serde_json::from_str(&raw).map_err(|_| rusqlite::Error::InvalidQuery)?;
                    Ok((
                        source,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, i64>(2)? as u64,
                        "managed".to_owned(),
                    ))
                },
            )
            .optional()
            .map_err(|_| AppError::Internal)
        })
        .await
        .map_err(|_| "data_unavailable".to_owned())?
        .ok_or_else(|| "not_found".into())
    }
    pub(super) fn missing_references(source: &SourceConfig) -> Vec<String> {
        source
            .headers_env
            .values()
            .filter(|name| std::env::var_os(name).is_none())
            .cloned()
            .collect()
    }
    pub(super) fn resolved_headers(source: &SourceConfig) -> Result<HeaderMap, String> {
        let mut headers = HeaderMap::new();
        for (name, env) in &source.headers_env {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| "config_error")?;
            let raw = std::env::var(env).map_err(|_| "config_error")?;
            let mut value = HeaderValue::from_str(&raw).map_err(|_| "config_error")?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        Ok(headers)
    }
    pub async fn public_sources(&self, org: Option<&str>) -> Vec<PublicSource> {
        let mut sources = self
            .sources
            .values()
            .filter(|s| org.is_none_or(|o| o == s.org))
            .cloned()
            .collect::<Vec<_>>();
        if let Ok(rows) = self.managed_rows().await {
            sources.extend(
                rows.into_iter()
                    .filter(|(s, enabled, _)| {
                        *enabled
                            && Self::missing_references(s).is_empty()
                            && org.is_none_or(|o| o == s.org)
                    })
                    .map(|(s, _, _)| s),
            );
        }
        sources.sort_by(|a, b| a.id.cmp(&b.id));
        sources
            .into_iter()
            .map(|s| PublicSource {
                id: s.id.clone(),
                kind: s.kind.clone(),
                operations: s
                    .operations
                    .iter()
                    .map(|(n, o)| {
                        (
                            n.clone(),
                            json!({"params":public_params(&o.params),"method":o.method,"stream":o.stream}),
                        )
                    })
                    .collect(),
                subscriptions: s
                    .subscriptions
                    .iter()
                    .map(|(n, o)| (n.clone(), json!({"transport":o.transport})))
                    .collect(),
            })
            .collect()
    }
    pub(super) async fn all_source_configs(
        &self,
    ) -> Result<BTreeMap<String, SourceConfig>, String> {
        let mut map = (*self.sources).clone();
        for (source, _, _) in self.managed_rows().await? {
            if map.contains_key(&source.id) {
                return Err("source_conflict".into());
            }
            map.insert(source.id.clone(), source);
        }
        Ok(map)
    }
    pub async fn validate_registry(&self) -> Result<(), String> {
        let mut ids = HashSet::new();
        for source in self.sources.values() {
            validate_source(source)?;
            ids.insert(source.id.clone());
        }
        let rows = self.managed_rows().await?;
        if self.sources.len() + rows.len() > MAX_SOURCES {
            return Err("invalid_data_sources".into());
        }
        for (source, _, _) in rows {
            validate_source(&source)?;
            if !ids.insert(source.id) {
                return Err("source_conflict".into());
            }
        }
        Ok(())
    }
}
pub(super) fn public_params(params: &BTreeMap<String, ParamConfig>) -> BTreeMap<String, Value> {
    params
        .iter()
        .map(|(name, p)| {
            let mut schema = serde_json::Map::new();
            schema.insert("type".into(), json!(p.kind));
            if p.required {
                schema.insert("required".into(), json!(true));
            }
            if let Some(value) = p.minimum {
                schema.insert("minimum".into(), json!(value));
            }
            if let Some(value) = p.maximum {
                schema.insert("maximum".into(), json!(value));
            }
            if p.kind == "string" {
                schema.insert("max_length".into(), json!(p.max_length.unwrap_or(256)));
            }
            if let Some(value) = &p.enum_values {
                schema.insert("enum".into(), json!(value));
            }
            if let Some(value) = &p.default {
                schema.insert("default".into(), value.clone());
            }
            (name.clone(), Value::Object(schema))
        })
        .collect()
}
