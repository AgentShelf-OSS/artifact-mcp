//! Schema introspection and string helpers shared by the migration bodies.
//!
//! `table_exists` is also used by the migration runner in the parent module.

use rusqlite::{Connection, Transaction};

/// `String.prototype.trim`'s character set, which is **not** Rust's `char::is_whitespace`.
///
/// ECMA-262 trims WhiteSpace ∪ LineTerminator: TAB, VT, FF, SP, NBSP, ZWNBSP (`U+FEFF`), every
/// `Zs`, LF, CR, LS, PS. Rust's White_Space property adds `U+0085` (NEL) and omits `U+FEFF`, so
/// both differences are corrected here.
#[must_use]
pub(super) const fn is_js_whitespace(value: char) -> bool {
    matches!(value, '\u{feff}') || (value.is_whitespace() && !matches!(value, '\u{85}'))
}

/// `String.prototype.trim` — [lib/migrations.js:225]
#[must_use]
pub(super) fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

pub(super) fn table_exists(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Port of `hasColumn` in `lib/migrations.js`.
pub(super) fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Port of `ensureColumn` in `lib/migrations.js`: additive, idempotent `ALTER TABLE`.
pub(super) fn ensure_column(
    tx: &Transaction<'_>,
    table: &str,
    column: &str,
    declaration: &str,
) -> rusqlite::Result<()> {
    if !has_column(tx, table, column)? {
        tx.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {declaration}"
        ))?;
    }
    Ok(())
}
