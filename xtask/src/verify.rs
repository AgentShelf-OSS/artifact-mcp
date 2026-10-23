//! `--verify-against-committed`: regenerate cases into a scratch directory and compare them with
//! the committed corpus.
//!
//! The comparison is logical where the bytes cannot match and exact everywhere else:
//!
//! * file tree: the same relative paths;
//! * bodies: every file under `artifacts/` byte for byte, and the body manifest;
//! * `fixture.json`: byte for byte after the provenance fields in [`PROVENANCE_FIELDS`] are
//!   copied from the committed manifest;
//! * `artifacts.db`: the logical header fields, `integrity_check`, every `sqlite_master` row
//!   (type, name, table, SQL text) in creation order, and every row of every table in rowid
//!   order. SQL text that differs is accepted only when it is equal after whitespace runs are
//!   collapsed (the Rust ledger re-indents some of Node's statements). Only columns whose
//!   `DEFAULT` reads the clock and that the generator does not write are masked, and only when
//!   both values are `YYYY-MM-DD HH:MM:SS` timestamps.
//!
//! Two committed boundaries were frozen from pre-release drafts of a migration. Their
//! differences are listed in [`FROZEN_DRAFT_DIVERGENCES`], reported separately, and do not fail
//! the run; any other difference does.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, types::Value};

use crate::Result;
use crate::fixture::{self, CaseSpec, GENERATION, Generated, Manifest, Outcome};

/// `fixture.json` fields that record when and how a fixture was made, not what it contains.
pub const PROVENANCE_FIELDS: &[(&str, &str)] = &[
    (
        "generation",
        "names the generator; committed fixtures say `node scripts/generate-historical-fixtures.mjs`",
    ),
    (
        "origin.sourceRef.migrationLedgerSha256",
        "digest of the ledger source at generation time; Node hashed lib/migrations.js, this \
         command hashes src/persistence/migrations.rs",
    ),
    (
        "expectedTargetSchema",
        "LATEST_SCHEMA_VERSION at generation time (24..=40 across the committed corpus)",
    ),
    (
        "database.sha256",
        "the SQLite file bytes differ: clock DEFAULT values and the SQLite library version in \
         the file header",
    ),
    (
        "expectedRecovery.remainingIntentIds",
        "only when the committed manifest predates the key (boundary-v00..v24) and the generated \
         value is []; u50 reads a missing key as []",
    ),
];

/// `sqlite_master` objects in which a committed boundary differs from the final ledger because
/// the fixture was frozen from a pre-release draft of that migration. The Node ledger at
/// `7211de0` (the last Node generator) has the final DDL too, so it could not reproduce these
/// objects either. Every other object of these cases is still compared, and the same objects in
/// the next committed boundary (v32, v37) equal the generated ones.
pub const FROZEN_DRAFT_DIVERGENCES: &[(&str, &str, &str)] = &[
    (
        "boundary-v31",
        "table discord_inbound_events",
        "draft migration 31 had no next_attempt_at and attempts columns",
    ),
    (
        "boundary-v31",
        "table discord_inbound_message_state",
        "draft migration 31 declared feedback_id NOT NULL",
    ),
    (
        "boundary-v31",
        "trigger discord_inbound_message_state_feedback_insert",
        "draft trigger had no NEW.feedback_id IS NOT NULL guard",
    ),
    (
        "boundary-v31",
        "trigger discord_inbound_message_state_feedback_update",
        "draft trigger had no NEW.feedback_id IS NOT NULL guard",
    ),
    (
        "boundary-v36",
        "table data_sources",
        "draft migration 36 had no datetime('now') defaults on created_at and updated_at",
    ),
    (
        "boundary-v36",
        "index data_sources_org_id",
        "draft migration 36 named the index data_sources_org_id, not data_sources_org_id_idx",
    ),
];

/// One difference. `object` is the `sqlite_master` object (`"<type> <name>"`) it concerns, if any.
pub struct Difference {
    pub object: Option<String>,
    pub message: String,
}

impl Difference {
    fn general(message: impl Into<String>) -> Self {
        Self {
            object: None,
            message: message.into(),
        }
    }

    fn object(object: String, message: impl Into<String>) -> Self {
        Self {
            object: Some(object),
            message: message.into(),
        }
    }
}

/// Result of one case comparison.
pub struct CaseReport {
    pub name: String,
    pub differences: Vec<Difference>,
    /// `table.column` values masked because the clock filled them.
    pub masked_columns: BTreeSet<String>,
    /// Provenance fields whose generated value differed from the committed value.
    pub provenance_substitutions: BTreeSet<String>,
    /// `sqlite_master` objects whose SQL text matches only after whitespace normalization.
    pub whitespace_only_sql: BTreeSet<String>,
}

impl CaseReport {
    fn fail(&mut self, message: impl Into<String>) {
        self.differences.push(Difference::general(message));
    }

    fn fail_object(&mut self, object: String, message: impl Into<String>) {
        self.differences.push(Difference::object(object, message));
    }

    /// Splits the differences into known frozen-draft divergences (with their reason) and
    /// unexpected ones.
    #[must_use]
    pub fn classify(&self) -> (Vec<(&Difference, &'static str)>, Vec<&Difference>) {
        let mut known = Vec::new();
        let mut unexpected = Vec::new();
        for difference in &self.differences {
            let reason = difference.object.as_deref().and_then(|object| {
                FROZEN_DRAFT_DIVERGENCES
                    .iter()
                    .find(|(case, known_object, _)| *case == self.name && *known_object == object)
                    .map(|(.., reason)| *reason)
            });
            match reason {
                Some(reason) => known.push((difference, reason)),
                None => unexpected.push(difference),
            }
        }
        (known, unexpected)
    }
}

/// Generates `spec` under `scratch` and compares it with `committed_root/<name>`.
pub fn verify_case(
    repo: &Path,
    committed_root: &Path,
    scratch: &Path,
    spec: &CaseSpec,
) -> Result<CaseReport> {
    let committed = committed_root.join(&spec.name);
    let mut report = CaseReport {
        name: spec.name.clone(),
        differences: Vec::new(),
        masked_columns: BTreeSet::new(),
        provenance_substitutions: BTreeSet::new(),
        whitespace_only_sql: BTreeSet::new(),
    };
    if !committed.is_dir() {
        report.fail(format!("no committed fixture at {}", committed.display()));
        return Ok(report);
    }
    let generated = match fixture::write_case(repo, scratch, spec, true)? {
        Outcome::Wrote(generated) => generated,
        Outcome::KeptExisting(path) => {
            return Err(format!("scratch case {} was not regenerated", path.display()).into());
        }
    };

    compare_tree(&committed, &generated.dir, &mut report)?;
    compare_manifest(repo, &committed, &generated, &mut report)?;
    compare_database(
        &committed.join("artifacts.db"),
        &generated.dir.join("artifacts.db"),
        &generated,
        &mut report,
    )?;
    Ok(report)
}

fn files(root: &Path) -> Result<BTreeSet<String>> {
    fn visit(base: &Path, dir: &Path, out: &mut BTreeSet<String>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                visit(base, &path, out)?;
            } else {
                out.insert(
                    path.strip_prefix(base)?
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
        Ok(())
    }
    let mut out = BTreeSet::new();
    visit(root, root, &mut out)?;
    Ok(out)
}

fn compare_tree(committed: &Path, generated: &Path, report: &mut CaseReport) -> Result<()> {
    let left = files(committed)?;
    let right = files(generated)?;
    for missing in left.difference(&right) {
        report.fail(format!("file {missing} is committed but was not generated"));
    }
    for extra in right.difference(&left) {
        report.fail(format!("file {extra} was generated but is not committed"));
    }
    for path in left.intersection(&right) {
        if path.starts_with("artifacts/")
            && fs::read(committed.join(path))? != fs::read(generated.join(path))?
        {
            report.fail(format!("body {path} differs byte for byte"));
        }
    }
    Ok(())
}

fn compare_manifest(
    repo: &Path,
    committed: &Path,
    generated: &Generated,
    report: &mut CaseReport,
) -> Result<()> {
    let committed_bytes = fs::read(committed.join("fixture.json"))?;
    let committed_manifest: Manifest = serde_json::from_slice(&committed_bytes)
        .map_err(|error| format!("committed fixture.json does not parse: {error}"))?;
    // Proof that this serializer reproduces `JSON.stringify(manifest, null, 2)` for the corpus.
    if committed_manifest.to_file_bytes()? != committed_bytes {
        report.fail("committed fixture.json does not round-trip through the manifest serializer");
    }

    let generated_bytes = fs::read(generated.dir.join("fixture.json"))?;
    let mut normalized: Manifest = serde_json::from_slice(&generated_bytes)?;
    if normalized != generated.manifest {
        report.fail("generated fixture.json does not match the generator's manifest");
    }

    // The generated provenance must be the current ledger, not merely "something".
    let ledger = fixture::ledger_digest(repo)?;
    let database_digest = fixture::sha256_hex(fs::read(generated.dir.join("artifacts.db"))?);
    let checks = [
        (normalized.generation == GENERATION, "generation"),
        (
            normalized.origin.source_ref.migration_ledger_sha256 == ledger,
            "origin.sourceRef.migrationLedgerSha256",
        ),
        (
            normalized.expected_target_schema
                == artifact_mcp::persistence::migrations::LATEST_SCHEMA_VERSION,
            "expectedTargetSchema",
        ),
        (
            normalized.database.sha256 == database_digest,
            "database.sha256",
        ),
        (
            normalized.bodies == fixture::body_manifest(&generated.dir)?,
            "bodies",
        ),
    ];
    for (ok, field) in checks {
        if !ok {
            report.fail(format!("generated {field} is not the current value"));
        }
    }

    let mut substitute = |field: &str, differs: bool| {
        if differs {
            report.provenance_substitutions.insert(field.to_owned());
        }
    };
    substitute(
        "generation",
        normalized.generation != committed_manifest.generation,
    );
    normalized
        .generation
        .clone_from(&committed_manifest.generation);
    substitute(
        "origin.sourceRef.migrationLedgerSha256",
        normalized.origin.source_ref.migration_ledger_sha256
            != committed_manifest.origin.source_ref.migration_ledger_sha256,
    );
    normalized
        .origin
        .source_ref
        .migration_ledger_sha256
        .clone_from(&committed_manifest.origin.source_ref.migration_ledger_sha256);
    substitute(
        "expectedTargetSchema",
        normalized.expected_target_schema != committed_manifest.expected_target_schema,
    );
    normalized.expected_target_schema = committed_manifest.expected_target_schema;
    substitute(
        "database.sha256",
        normalized.database.sha256 != committed_manifest.database.sha256,
    );
    normalized
        .database
        .sha256
        .clone_from(&committed_manifest.database.sha256);
    if committed_manifest
        .expected_recovery
        .remaining_intent_ids
        .is_none()
        && normalized.expected_recovery.remaining_intent_ids == Some(Vec::new())
    {
        substitute("expectedRecovery.remainingIntentIds", true);
        normalized.expected_recovery.remaining_intent_ids = None;
    }

    if normalized.to_file_bytes()? != committed_bytes {
        let left: serde_json::Value = serde_json::from_slice(&committed_bytes)?;
        let right = serde_json::to_value(&normalized)?;
        report.fail(format!(
            "fixture.json differs outside the provenance fields:\n  committed: {left}\n  generated: {right}"
        ));
    }
    Ok(())
}

/// Header fields that describe the logical database, as `(offset, length, name)`.
///
/// Excluded as physical: change counter (24), page count (28), freelist (32, 36), schema cookie
/// (40), the SQLite version that last wrote the file (92, 96).
const HEADER_FIELDS: &[(usize, usize, &str)] = &[
    (0, 16, "magic"),
    (16, 2, "page size"),
    (18, 1, "write format (1 rollback journal, 2 WAL)"),
    (19, 1, "read format (1 rollback journal, 2 WAL)"),
    (44, 4, "schema format"),
    (52, 4, "auto-vacuum root"),
    (56, 4, "text encoding"),
    (60, 4, "user_version"),
    (64, 4, "incremental vacuum"),
    (68, 4, "application_id"),
];

fn open_read_only(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

type MasterRow = (String, String, String, Option<String>);

fn master_rows(conn: &Connection) -> Result<Vec<MasterRow>> {
    let mut statement =
        conn.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY rowid")?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Collapses runs of whitespace, exactly like `tests/native/u03_support.rs::normalize_sql`.
fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Columns whose `DEFAULT` reads the clock.
fn clock_columns(conn: &Connection, table: &str) -> Result<BTreeSet<String>> {
    let mut statement = conn.prepare("SELECT name, dflt_value FROM pragma_table_info(?1)")?;
    let rows = statement
        .query_map([table], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, default)| {
            default.as_deref().is_some_and(|value| {
                let value = value.to_ascii_lowercase();
                value.contains("'now'") || value.contains("current_")
            })
        })
        .map(|(name, _)| name)
        .collect())
}

fn is_timestamp(value: &Value) -> bool {
    let Value::Text(text) = value else {
        return false;
    };
    let bytes = text.as_bytes();
    bytes.len() == 19
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 => *byte == b'-',
            10 => *byte == b' ',
            13 | 16 => *byte == b':',
            _ => byte.is_ascii_digit(),
        })
}

/// Rows of `table` with their column names, in rowid order (or sorted for WITHOUT ROWID tables).
fn table_rows(conn: &Connection, table: &str) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    let quoted = format!("\"{}\"", table.replace('"', "\"\""));
    let (mut statement, sorted) =
        match conn.prepare(&format!("SELECT rowid, * FROM {quoted} ORDER BY rowid")) {
            Ok(statement) => (statement, true),
            Err(_) => (conn.prepare(&format!("SELECT * FROM {quoted}"))?, false),
        };
    let names = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let width = names.len();
    let mut rows = statement
        .query_map([], |row| {
            (0..width)
                .map(|index| row.get::<_, Value>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !sorted {
        rows.sort_by_key(|row| format!("{row:?}"));
    }
    Ok((names, rows))
}

fn compare_database(
    committed: &Path,
    generated_db: &Path,
    generated: &Generated,
    report: &mut CaseReport,
) -> Result<()> {
    let left_bytes = fs::read(committed)?;
    let right_bytes = fs::read(generated_db)?;
    for (offset, length, name) in HEADER_FIELDS {
        let range = *offset..offset + length;
        if left_bytes.get(range.clone()) != right_bytes.get(range) {
            report.fail(format!("database header field {name} differs"));
        }
    }

    let left = open_read_only(committed)?;
    let right = open_read_only(generated_db)?;
    for (label, conn) in [("committed", &left), ("generated", &right)] {
        let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            report.fail(format!("{label} integrity_check: {integrity}"));
        }
    }
    let violations: i64 =
        right.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if violations != 0 {
        report.fail(format!(
            "generated database has {violations} foreign key violations"
        ));
    }

    let left_master = master_rows(&left)?;
    let right_master = master_rows(&right)?;
    if left_master.len() != right_master.len() {
        report.fail(format!(
            "sqlite_master has {} committed and {} generated rows",
            left_master.len(),
            right_master.len()
        ));
    }
    for (committed_row, generated_row) in left_master.iter().zip(&right_master) {
        if committed_row == generated_row {
            continue;
        }
        // The Rust ledger keeps Node's statements but not always Node's indentation, so compare
        // with whitespace runs collapsed, as `u03_support::normalize_sql` does.
        let (kind, name, table, sql) = committed_row;
        let (other_kind, other_name, other_table, other_sql) = generated_row;
        if kind == other_kind
            && name == other_name
            && table == other_table
            && sql.as_deref().map(normalize_sql) == other_sql.as_deref().map(normalize_sql)
        {
            report.whitespace_only_sql.insert(format!("{kind} {name}"));
        } else {
            report.fail_object(
                format!("{kind} {name}"),
                format!(
                    "sqlite_master differs:\n  committed: {committed_row:?}\n  generated: {generated_row:?}"
                ),
            );
        }
    }

    let tables = left_master
        .iter()
        .filter(|(kind, ..)| kind == "table")
        .map(|(_, name, ..)| name.clone())
        .collect::<Vec<_>>();
    let explicit = generated.explicit_columns.iter().fold(
        BTreeMap::<&str, BTreeSet<&str>>::new(),
        |mut map, (table, column)| {
            map.entry(table.as_str())
                .or_default()
                .insert(column.as_str());
            map
        },
    );
    for table in tables {
        let (left_names, mut left_rows) = table_rows(&left, &table)?;
        let (right_names, mut right_rows) = table_rows(&right, &table)?;
        if left_names != right_names {
            // A consequence of a `CREATE TABLE` difference, so it is filed under that object.
            // The row count is still compared.
            report.fail_object(
                format!("table {table}"),
                format!("{table}: column lists differ"),
            );
            if left_rows.len() != right_rows.len() {
                report.fail(format!(
                    "{table}: {} committed and {} generated rows",
                    left_rows.len(),
                    right_rows.len()
                ));
            }
            continue;
        }
        let written = explicit.get(table.as_str());
        let masked = clock_columns(&left, &table)?
            .into_iter()
            .filter(|column| !written.is_some_and(|set| set.contains(column.as_str())))
            .collect::<BTreeSet<_>>();
        for (index, name) in left_names.iter().enumerate() {
            if !masked.contains(name) {
                continue;
            }
            for rows in [&mut left_rows, &mut right_rows] {
                for row in rows.iter_mut() {
                    if is_timestamp(&row[index]) {
                        row[index] = Value::Text("<clock default>".to_owned());
                        report.masked_columns.insert(format!("{table}.{name}"));
                    }
                }
            }
        }
        if left_rows != right_rows {
            report.fail(format!(
                "{table}: rows differ\n  committed: {left_rows:?}\n  generated: {right_rows:?}"
            ));
        }
    }
    Ok(())
}

/// A scratch directory under the system temporary directory, removed on drop.
pub struct Scratch {
    pub path: PathBuf,
    /// `true` when the caller chose the directory with `--out`; it is then left in place.
    pub keep: bool,
}

impl Scratch {
    /// Uses `out` (kept afterwards) or a new temporary directory (removed afterwards).
    pub fn new(out: Option<PathBuf>) -> Result<Self> {
        if let Some(path) = out {
            fs::create_dir_all(&path)?;
            return Ok(Self { path, keep: true });
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "artifact-mcp-xtask-verify-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path, keep: false })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
