//! Organization-scoped viewer state persistence.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::{
    error::AppError,
    mcp::protocol::OrderedJson,
    model::{ArtifactId, EmailAddress, Timestamp},
    persistence::db::{self, DbPool},
};

pub use crate::config::{
    STATE_MAX_KEY_BYTES as MAX_KEY_BYTES, STATE_MAX_KEYS as MAX_KEYS,
    STATE_MAX_VALUE_BYTES as MAX_VALUE_BYTES,
};

#[derive(Clone, Debug, PartialEq)]
pub struct StateValue {
    pub artifact_id: ArtifactId,
    pub key: String,
    pub value: OrderedJson,
    pub revision: u64,
    pub updated_at: Timestamp,
    pub updated_by: EmailAddress,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateKey {
    pub key: String,
    pub revision: u64,
    pub updated_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    pub value: OrderedJson,
    pub revision: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("{0}")]
    App(#[from] AppError),
    #[error("state conflict")]
    Conflict(Conflict),
    #[error("too many state keys")]
    TooManyKeys,
}

fn internal(context: &str, error: impl std::fmt::Display) -> StateError {
    let _ = (context, error);
    StateError::App(AppError::Internal)
}

fn revision(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    value
        .try_into()
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn decode(row: &rusqlite::Row<'_>, artifact_id: &ArtifactId) -> rusqlite::Result<StateValue> {
    let raw: String = row.get(2)?;
    Ok(StateValue {
        artifact_id: artifact_id.clone(),
        key: row.get(1)?,
        value: serde_json::from_str(&raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        revision: revision(row, 3)?,
        updated_at: Timestamp(row.get(4)?),
        updated_by: EmailAddress(row.get(5)?),
    })
}

pub fn list(conn: &Connection, artifact_id: &ArtifactId) -> Result<Vec<StateKey>, AppError> {
    let mut stmt = conn.prepare("SELECT key, revision, updated_at FROM artifact_state WHERE artifact_id = ? ORDER BY key")
        .map_err(|_| AppError::Internal)?;
    let rows = stmt
        .query_map([&artifact_id.0], |row| {
            Ok(StateKey {
                key: row.get(0)?,
                revision: revision(row, 1)?,
                updated_at: Timestamp(row.get(2)?),
            })
        })
        .map_err(|_| AppError::Internal)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|_| AppError::Internal)
}

pub fn get(
    conn: &Connection,
    artifact_id: &ArtifactId,
    key: &str,
) -> Result<Option<StateValue>, AppError> {
    conn.query_row("SELECT artifact_id,key,value,revision,updated_at,updated_by FROM artifact_state WHERE artifact_id=? AND key=?", params![artifact_id.0, key], |row| decode(row, artifact_id))
        .optional().map_err(|_| AppError::Internal)
}

pub fn set(
    conn: &mut Connection,
    artifact_id: &ArtifactId,
    key: &str,
    value: &OrderedJson,
    if_revision: Option<u64>,
    updated_by: &EmailAddress,
) -> Result<StateValue, StateError> {
    if !valid_key(key) {
        return Err(StateError::App(AppError::Validation("bad key".into())));
    }
    let encoded = value
        .to_json_string()
        .map_err(|_| StateError::App(AppError::Validation("invalid value".into())))?;
    if encoded.len() > MAX_VALUE_BYTES {
        return Err(StateError::App(AppError::PayloadTooLarge));
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| internal("begin state transaction", e))?;
    let current = tx
        .query_row(
            "SELECT value,revision FROM artifact_state WHERE artifact_id=? AND key=?",
            params![artifact_id.0, key],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|e| internal("read state", e))?;
    if let Some(expected) = if_revision {
        let actual = current
            .as_ref()
            .map_or(0, |(_, revision)| u64::try_from(*revision).unwrap_or(0));
        if expected != actual {
            let current_value = current
                .as_ref()
                .and_then(|(raw, _)| serde_json::from_str(raw).ok())
                .unwrap_or(OrderedJson::Null);
            return Err(StateError::Conflict(Conflict {
                value: current_value,
                revision: actual,
            }));
        }
    }
    if current.is_none() {
        let count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM artifact_state WHERE artifact_id=?",
                [&artifact_id.0],
                |row| row.get(0),
            )
            .map_err(|e| internal("count state keys", e))?;
        if count >= MAX_KEYS {
            return Err(StateError::TooManyKeys);
        }
    }
    tx.execute("INSERT INTO artifact_state (artifact_id,key,value,revision,updated_at,updated_by) VALUES (?, ?, ?, 1, datetime('now'), ?) ON CONFLICT(artifact_id,key) DO UPDATE SET value=excluded.value, revision=artifact_state.revision+1, updated_at=datetime('now'), updated_by=excluded.updated_by", params![artifact_id.0, key, encoded, updated_by.0]).map_err(|e| internal("write state", e))?;
    let result = tx.query_row("SELECT artifact_id,key,value,revision,updated_at,updated_by FROM artifact_state WHERE artifact_id=? AND key=?", params![artifact_id.0, key], |row| decode(row, artifact_id)).map_err(|e| internal("read written state", e))?;
    tx.commit().map_err(|e| internal("commit state", e))?;
    Ok(result)
}

pub fn delete(conn: &Connection, artifact_id: &ArtifactId, key: &str) -> Result<bool, AppError> {
    conn.execute(
        "DELETE FROM artifact_state WHERE artifact_id=? AND key=?",
        params![artifact_id.0, key],
    )
    .map(|n| n != 0)
    .map_err(|_| AppError::Internal)
}

pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_BYTES
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub async fn list_pooled(pool: &DbPool, id: ArtifactId) -> Result<Vec<StateKey>, AppError> {
    db::interact(pool, move |conn| list(conn, &id)).await
}
pub async fn get_pooled(
    pool: &DbPool,
    id: ArtifactId,
    key: String,
) -> Result<Option<StateValue>, AppError> {
    db::interact(pool, move |conn| get(conn, &id, &key)).await
}
pub async fn set_pooled(
    pool: &DbPool,
    id: ArtifactId,
    key: String,
    value: OrderedJson,
    if_revision: Option<u64>,
    writer: EmailAddress,
) -> Result<StateValue, StateError> {
    let pool = pool.clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = db::checkout(&pool).map_err(StateError::App)?;
        set(&mut conn, &id, &key, &value, if_revision, &writer)
    })
    .await
    .map_err(|_| StateError::App(AppError::Internal))?
}
pub async fn delete_pooled(pool: &DbPool, id: ArtifactId, key: String) -> Result<bool, AppError> {
    db::interact(pool, move |conn| delete(conn, &id, &key)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_match_the_bridge_contract() {
        assert!(valid_key("highlights.v1"));
        assert!(valid_key(&"a".repeat(MAX_KEY_BYTES)));
        assert!(!valid_key(""));
        assert!(!valid_key(&"a".repeat(MAX_KEY_BYTES + 1)));
        assert!(!valid_key("contains space"));
        assert!(!valid_key("é"));
    }

    #[test]
    fn crud_increments_revision_and_detects_conflicts() {
        let mut conn = Connection::open_in_memory().expect("sqlite");
        conn.execute_batch("CREATE TABLE artifacts (id TEXT PRIMARY KEY); CREATE TABLE artifact_state (artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE, key TEXT NOT NULL, value TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 1, updated_at TEXT NOT NULL, updated_by TEXT NOT NULL, PRIMARY KEY (artifact_id,key)); INSERT INTO artifacts VALUES ('a'); PRAGMA foreign_keys=ON;").expect("schema");
        let id = ArtifactId("a".into());
        let email = EmailAddress("viewer@example.test".into());
        let first = set(
            &mut conn,
            &id,
            "note",
            &OrderedJson::string("one"),
            None,
            &email,
        )
        .expect("first");
        assert_eq!(first.revision, 1);
        let second = set(
            &mut conn,
            &id,
            "note",
            &OrderedJson::string("two"),
            Some(1),
            &email,
        )
        .expect("second");
        assert_eq!(second.revision, 2);
        let StateError::Conflict(conflict) = set(
            &mut conn,
            &id,
            "note",
            &OrderedJson::string("three"),
            Some(1),
            &email,
        )
        .expect_err("stale revision") else {
            panic!("expected conflict")
        };
        assert_eq!(conflict.revision, 2);
        assert_eq!(conflict.value, OrderedJson::string("two"));
        assert!(delete(&conn, &id, "note").expect("delete"));
        assert!(get(&conn, &id, "note").expect("read").is_none());
    }
}
