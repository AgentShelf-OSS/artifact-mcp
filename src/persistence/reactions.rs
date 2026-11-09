//! Reaction persistence.
//!
//! One row per `(email, artifact_id)` holds a viewer's *favorite* flag and their sentiment *vote*. Both are constrained by the v3 `reaction-integrity` migration (`favorite IN (0, 1)`, `vote IN (-1, 0, 1)`) and the row is deleted with its artifact by `FOREIGN KEY (artifact_id) REFERENCES artifacts(id) ON DELETE CASCADE`.
//!
//! # Semantics preserved
//!
//! * **Absent row reads as neutral.** The result is `{ favorite: 0, vote: 0 }` when no row exists; there is no "unset" state visible to callers.
//! * **Partial update.** Read the current row first and only replace the field(s) supplied, then upsert *both* columns. A missing field keeps its stored value.
//! * **Upsert, never duplicate.** `ON CONFLICT(email, artifact_id) DO UPDATE` keeps exactly one row per viewer per artifact.
//! * **Vote normalisation.** Clamp votes to `-1`, `0`, or `1` *after* rejecting values outside that set. Both checks preserve the frozen contract: [`validate`] returns the exact validation message and [`normalize_vote`] clamps by sign, so no unchecked value can reach the `CHECK` constraint.
//!
//! # Concurrency note (difference in mechanism, not in behaviour)
//!
//! The retired Node twin serialized read-modify-write operations through one process-wide SQLite connection. The Rust pool has up to four connections, so [`set`] reads and upserts inside an `IMMEDIATE` transaction to preserve that behavior. Without it two concurrent partial updates could interleave and lose a field.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::error::AppError;
use crate::model::{ArtifactId, EmailAddress, Reaction, ReactionUpdate, Sentiment};
use crate::persistence::db::{self, DbPool};

/// Frozen rejection message for an out-of-range `favorite`.
pub const FAVORITE_VALUE_MESSAGE: &str = "favorite must be true, false, 0, or 1.";

/// Frozen rejection message for an out-of-range `vote`.
pub const VOTE_VALUE_MESSAGE: &str = "vote must be -1, 0, or 1.";

/// The only vote values the v3 `CHECK` constraint accepts.
pub const ALLOWED_VOTES: [i8; 3] = [-1, 0, 1];

const GET_SQL: &str = "SELECT favorite, vote FROM reactions WHERE email = ? AND artifact_id = ?";

const UPSERT_SQL: &str = "\
INSERT INTO reactions (email, artifact_id, favorite, vote, updated_at)
VALUES (?, ?, ?, ?, datetime('now'))
ON CONFLICT(email, artifact_id) DO UPDATE SET favorite = excluded.favorite, vote = excluded.vote, updated_at = datetime('now')";

const MINE_SQL: &str = "SELECT artifact_id, favorite, vote FROM reactions WHERE email = ?";

const SENTIMENT_SQL: &str = "\
SELECT artifact_id,
       SUM(CASE WHEN vote = 1 THEN 1 ELSE 0 END)  AS up,
       SUM(CASE WHEN vote = -1 THEN 1 ELSE 0 END) AS down,
       SUM(favorite)                              AS favorites
FROM reactions GROUP BY artifact_id";

/// Rejects an update outside the frozen reaction input contract.
///
/// `favorite` is already a `bool` in [`ReactionUpdate`], so only `vote` can carry an illegal
/// value; preserve the validation message byte-for-byte so the HTTP envelope matches.
///
/// # Errors
/// Returns [`AppError::Validation`] when `vote` is outside `{-1, 0, 1}`.
pub fn validate(update: &ReactionUpdate) -> Result<(), AppError> {
    match update.vote {
        Some(vote) if !ALLOWED_VOTES.contains(&vote) => {
            Err(AppError::Validation(VOTE_VALUE_MESSAGE.to_owned()))
        }
        _ => Ok(()),
    }
}

/// Clamp a vote by its sign to -1, 0, or 1.
#[must_use]
pub const fn normalize_vote(vote: i8) -> i8 {
    if vote > 0 {
        1
    } else if vote < 0 {
        -1
    } else {
        0
    }
}

/// Normalize a favorite flag to 0 or 1.
#[must_use]
pub const fn normalize_favorite(favorite: bool) -> i8 {
    if favorite { 1 } else { 0 }
}

/// A viewer's reaction to one artifact; a missing row reads as `{ favorite: 0, vote: 0 }`.
///
/// # Errors
/// Returns [`AppError::Internal`] if the query fails.
pub fn get(
    conn: &Connection,
    email: &EmailAddress,
    artifact_id: &ArtifactId,
) -> Result<Reaction, AppError> {
    let stored = conn
        .query_row(GET_SQL, (&email.0, &artifact_id.0), |row| {
            Ok(Reaction {
                favorite: row.get(0)?,
                vote: row.get(1)?,
            })
        })
        .optional()
        .map_err(|error| crate::error::internal("read reaction", error))?;
    Ok(stored.unwrap_or_default())
}

/// Applies a partial reaction update and returns the stored result.
///
/// Fields left as `None` keep their current value.
///
/// # Errors
/// Returns [`AppError::Validation`] for an out-of-range vote, or [`AppError::Internal`] if the
/// transaction fails. A vanished artifact trips the foreign key and reports `Internal`; the route
/// authorises the artifact first, so that is a lost race, not a reachable request shape.
pub fn set(
    conn: &mut Connection,
    email: &EmailAddress,
    artifact_id: &ArtifactId,
    update: ReactionUpdate,
) -> Result<Reaction, AppError> {
    validate(&update)?;

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| crate::error::internal("begin reaction transaction", error))?;

    let current = get(&tx, email, artifact_id)?;
    let next = Reaction {
        favorite: update.favorite.map_or(current.favorite, normalize_favorite),
        vote: update.vote.map_or(current.vote, normalize_vote),
    };
    tx.execute(
        UPSERT_SQL,
        (&email.0, &artifact_id.0, next.favorite, next.vote),
    )
    .map_err(|error| crate::error::internal("upsert reaction", error))?;
    tx.commit()
        .map_err(|error| crate::error::internal("commit reaction", error))?;

    Ok(next)
}

/// Every reaction a single viewer has recorded, keyed by artifact.
///
/// Only keyed lookup is observable; the frozen `BTreeMap` return type
/// does not need to preserve database row order.
///
/// # Errors
/// Returns [`AppError::Internal`] if the query fails.
pub fn for_viewer(
    conn: &Connection,
    email: &EmailAddress,
) -> Result<BTreeMap<ArtifactId, Reaction>, AppError> {
    let mut stmt = conn
        .prepare(MINE_SQL)
        .map_err(|error| crate::error::internal("prepare viewer reactions", error))?;
    let rows = stmt
        .query_map((&email.0,), |row| {
            Ok((
                ArtifactId(row.get(0)?),
                Reaction {
                    favorite: row.get(1)?,
                    vote: row.get(2)?,
                },
            ))
        })
        .map_err(|error| crate::error::internal("query viewer reactions", error))?;

    let mut map = BTreeMap::new();
    for row in rows {
        let (id, reaction) =
            row.map_err(|error| crate::error::internal("read viewer reaction", error))?;
        map.insert(id, reaction);
    }
    Ok(map)
}

/// Aggregate sentiment across every viewer.
///
/// # Errors
/// Returns [`AppError::Internal`] if the query fails.
pub fn sentiment(conn: &Connection) -> Result<BTreeMap<ArtifactId, Sentiment>, AppError> {
    let mut stmt = conn
        .prepare(SENTIMENT_SQL)
        .map_err(|error| crate::error::internal("prepare sentiment", error))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                ArtifactId(row.get(0)?),
                Sentiment {
                    up: counter(row.get(1)?),
                    down: counter(row.get(2)?),
                    favorites: counter(row.get(3)?),
                },
            ))
        })
        .map_err(|error| crate::error::internal("query sentiment", error))?;

    let mut map = BTreeMap::new();
    for row in rows {
        let (id, aggregate) =
            row.map_err(|error| crate::error::internal("read sentiment", error))?;
        map.insert(id, aggregate);
    }
    Ok(map)
}

/// Pooled [`get`], run on the blocking pool through [`db::interact`].
///
/// # Errors
/// See [`get`]; also [`AppError::Unavailable`] when no connection is available.
pub async fn get_pooled(
    pool: &DbPool,
    email: EmailAddress,
    artifact_id: ArtifactId,
) -> Result<Reaction, AppError> {
    db::interact(pool, move |conn| get(conn, &email, &artifact_id)).await
}

/// Pooled [`set`].
///
/// # Errors
/// See [`set`]; also [`AppError::Unavailable`] when no connection is available.
pub async fn set_pooled(
    pool: &DbPool,
    email: EmailAddress,
    artifact_id: ArtifactId,
    update: ReactionUpdate,
) -> Result<Reaction, AppError> {
    db::interact(pool, move |conn| set(conn, &email, &artifact_id, update)).await
}

/// Pooled [`for_viewer`].
///
/// # Errors
/// See [`for_viewer`]; also [`AppError::Unavailable`] when no connection is available.
pub async fn for_viewer_pooled(
    pool: &DbPool,
    email: EmailAddress,
) -> Result<BTreeMap<ArtifactId, Reaction>, AppError> {
    db::interact(pool, move |conn| for_viewer(conn, &email)).await
}

/// Pooled [`sentiment`].
///
/// # Errors
/// See [`sentiment`]; also [`AppError::Unavailable`] when no connection is available.
pub async fn sentiment_pooled(pool: &DbPool) -> Result<BTreeMap<ArtifactId, Sentiment>, AppError> {
    db::interact(pool, |conn| sentiment(conn)).await
}

/// SQLite counts are `i64`; `rusqlite` only reads `u64` behind an optional feature this crate
/// does not enable. `SUM`/`COUNT` over the non-negative reaction columns cannot go negative, so a
/// negative value would mean a hand-corrupted row — reported as zero rather than a panic.
const fn counter(value: i64) -> u64 {
    if value < 0 { 0 } else { value.unsigned_abs() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_votes_the_way_node_does() {
        assert_eq!(normalize_vote(5), 1);
        assert_eq!(normalize_vote(1), 1);
        assert_eq!(normalize_vote(0), 0);
        assert_eq!(normalize_vote(-1), -1);
        assert_eq!(normalize_vote(-9), -1);
        assert_eq!(normalize_favorite(true), 1);
        assert_eq!(normalize_favorite(false), 0);
    }

    #[test]
    fn rejects_votes_outside_the_check_constraint() {
        for vote in [-2_i8, 2, 7, -128, 127] {
            let error = validate(&ReactionUpdate {
                favorite: None,
                vote: Some(vote),
            })
            .expect_err("out-of-range vote must be rejected");
            assert_eq!(error, AppError::Validation(VOTE_VALUE_MESSAGE.to_owned()));
        }
        for vote in ALLOWED_VOTES {
            validate(&ReactionUpdate {
                favorite: None,
                vote: Some(vote),
            })
            .expect("allowed vote");
        }
        validate(&ReactionUpdate {
            favorite: Some(true),
            vote: None,
        })
        .expect("favorite-only update");
    }
}
