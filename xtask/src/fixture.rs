//! Synthetic historical fixtures: the Rust port of the retired
//! `scripts/generate-historical-fixtures.mjs` (last present at commit `7211de0`).
//!
//! A case directory holds `artifacts.db`, the artifact bodies under `artifacts/`, and
//! `fixture.json`, which records every byte that was written. The rows, bodies, constants, and
//! manifest fields match the Node generator so that the corpus stays uniform. Only the provenance
//! changes: `generation` names this command, and `migrationLedgerSha256` is the SHA-256 of
//! `src/persistence/migrations.rs` (the Node generator hashed `lib/migrations.js`).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{AeadInPlace, KeyInit},
};
use artifact_mcp::persistence::migrations::{self, LATEST_SCHEMA_VERSION, MigrationContext};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rusqlite::{Connection, types::Value};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Result;

/// 32 synthetic zero bytes. Public test material, never a deployment key.
pub const FIXTURE_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
/// Public test material, never a deployment credential.
pub const FIXTURE_API_TOKEN: &str = "fixture-public-token-v1";
/// Value of `fixture.json` `generation` for fixtures this command writes.
pub const GENERATION: &str = "cargo xtask historical-fixture";
/// The migration ledger whose digest `fixture.json` records, relative to the repository root.
pub const LEDGER_SOURCE: &str = "src/persistence/migrations.rs";
const LEDGER_REF: &str = "current append-only migration ledger";
const FIXTURE_OWNER: &str = "fixture-owner@example.test";
const FIXTURE_DESCRIPTION: &str = "Synthetic historical fixture";

/// Pre-ledger (schema 0) layout, byte for byte as the Node generator executed it. SQLite stores
/// the statement text in `sqlite_master`, so the line breaks and indentation are part of the
/// fixture.
const PRE_LEDGER_LAYOUT: &str = concat!(
    "CREATE TABLE api_keys (client_id TEXT PRIMARY KEY, key_hash TEXT NOT NULL UNIQUE,\n",
    "        created_at TEXT NOT NULL DEFAULT (datetime('now')), revoked_at TEXT);\n",
    "        CREATE TABLE artifacts (id TEXT PRIMARY KEY, client_id TEXT NOT NULL, title TEXT NOT NULL,\n",
    "        description TEXT NOT NULL DEFAULT '', bytes INTEGER NOT NULL DEFAULT 0,\n",
    "        created_at TEXT NOT NULL DEFAULT (datetime('now')), updated_at TEXT NOT NULL DEFAULT (datetime('now')));\n",
    "        CREATE TABLE reactions (email TEXT NOT NULL, artifact_id TEXT NOT NULL, favorite INTEGER NOT NULL DEFAULT 0,\n",
    "        vote INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL DEFAULT (datetime('now')),\n",
    "        PRIMARY KEY (email, artifact_id));",
);

const CURRENT_SINGLE: &str = "<main>current single v2</main>";
const HISTORICAL_SINGLE: &str = "<main>single revision one</main>";
const BUNDLE_INDEX: &str = "<main>historical bundle</main>";
const BUNDLE_CSS: &str = "main { color: #3569a8; }\n";

/// Webhook rows a case seeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebhookMode {
    /// No webhook rows.
    None,
    /// One plaintext and one AES-GCM-encrypted Discord webhook.
    Mixed,
}

/// One fixture case. Field names follow the `CASES` entries of the Node generator.
#[derive(Clone, Debug)]
pub struct CaseSpec {
    pub name: String,
    pub origin_schema: i64,
    pub boundary: bool,
    pub webhook_mode: WebhookMode,
    pub recovery: bool,
    pub durability_recovery: bool,
}

/// Name of the one rich case that the current ledger (not a release tag) produced.
pub const DURABILITY_RECOVERY_CASE: &str = "release-v24-durability-recovery";

/// Rich cases that a historical release tag's Node `lib/migrations.js` produced. They are frozen
/// evidence and cannot be regenerated from the Rust ledger.
pub const TAG_RELEASE_CASES: &[(&str, &str)] = &[
    ("release-v16", "v1.2.0"),
    ("release-v20", "v1.4.0"),
    ("release-v21", "v1.5.0"),
    ("release-v23-recovery", "v1.6.0"),
];

impl CaseSpec {
    /// `boundary-vNN`: the current ledger through schema `NN`, or the pre-ledger layout for `0`.
    #[must_use]
    pub fn boundary(origin_schema: i64) -> Self {
        Self {
            name: boundary_name(origin_schema),
            origin_schema,
            boundary: true,
            webhook_mode: WebhookMode::None,
            recovery: false,
            durability_recovery: false,
        }
    }

    /// `release-v24-durability-recovery`: the current ledger through schema 24 with rich recovery
    /// state. It is not built from a release tag, so it can be regenerated and verified.
    #[must_use]
    pub fn durability_recovery() -> Self {
        Self {
            name: DURABILITY_RECOVERY_CASE.to_owned(),
            origin_schema: 24,
            boundary: false,
            webhook_mode: WebhookMode::Mixed,
            recovery: true,
            durability_recovery: true,
        }
    }

    fn kind(&self) -> &'static str {
        if self.boundary {
            "migration-boundary"
        } else {
            "current-ledger-recovery"
        }
    }
}

/// `boundary-vNN` with at least two digits, as the Node generator named it.
#[must_use]
pub fn boundary_name(schema: i64) -> String {
    format!("boundary-v{schema:02}")
}

/// Every case this command can build: one boundary per schema version, then the current-ledger
/// recovery case.
#[must_use]
pub fn generatable_cases() -> Vec<CaseSpec> {
    let mut cases = (0..=LATEST_SCHEMA_VERSION)
        .map(CaseSpec::boundary)
        .collect::<Vec<_>>();
    cases.push(CaseSpec::durability_recovery());
    cases
}

/// Builds the case named `name`, or explains why it cannot be built.
pub fn case_by_name(name: &str) -> Result<CaseSpec> {
    if let Some((_, tag)) = TAG_RELEASE_CASES.iter().find(|(case, _)| *case == name) {
        return Err(format!(
            "{name} is frozen evidence from release tag {tag}'s Node lib/migrations.js; it cannot \
             be regenerated from the Rust ledger"
        )
        .into());
    }
    generatable_cases()
        .into_iter()
        .find(|case| case.name == name)
        .ok_or_else(|| {
            format!(
                "unknown fixture case {name}; expected boundary-v00..={} or {DURABILITY_RECOVERY_CASE}",
                boundary_name(LATEST_SCHEMA_VERSION)
            )
            .into()
        })
}

// ---------------------------------------------------------------------------------------------
// fixture.json
// ---------------------------------------------------------------------------------------------

/// `fixture.json`. Field order is the Node generator's object literal order, and
/// `serde_json::to_string_pretty` plus a newline reproduces `JSON.stringify(manifest, null, 2)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub synthetic: bool,
    pub origin: Origin,
    pub expected_target_schema: i64,
    pub generation: String,
    pub webhook_encryption: Option<WebhookEncryption>,
    pub authentication: Authentication,
    pub expected_recovery: ExpectedRecovery,
    pub database: DatabaseRef,
    pub bodies: Vec<BodyEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Origin {
    pub release_tag: Option<String>,
    pub schema_version: i64,
    pub kind: String,
    pub source_ref: SourceRef,
}

/// Provenance of a current-ledger case. The field name is unchanged from the Node generator; the
/// digest now covers [`LEDGER_SOURCE`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceRef {
    #[serde(rename = "ref")]
    pub reference: String,
    pub migration_ledger_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebhookEncryption {
    pub key_id: String,
    pub key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Authentication {
    pub client_id: String,
    pub token: String,
    pub classification: String,
}

/// What storage reconciliation must report after the fixture boots.
///
/// `remainingIntentIds` is optional only so that older committed manifests (which predate the
/// key) parse and re-serialize byte for byte. This generator always writes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExpectedRecovery {
    pub recovered_paths: Vec<String>,
    pub divergent_bodies: Vec<String>,
    pub orphan_bodies: Vec<String>,
    pub missing_bodies: Vec<String>,
    pub preserved_transient_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_intent_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_metadata_only: Option<PreparedMetadataOnly>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreparedMetadataOnly {
    pub id: String,
    pub revision: u64,
    pub history_revision: u64,
    pub html: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DatabaseRef {
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BodyEntry {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

impl Manifest {
    /// The exact `fixture.json` bytes.
    pub fn to_file_bytes(&self) -> Result<Vec<u8>> {
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        Ok(text.into_bytes())
    }
}

// ---------------------------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------------------------

/// Result of [`write_case`].
pub enum Outcome {
    /// The case directory already existed and was left untouched.
    KeptExisting(PathBuf),
    /// The case was (re)generated.
    Wrote(Box<Generated>),
}

/// A freshly generated case.
pub struct Generated {
    pub dir: PathBuf,
    pub manifest: Manifest,
    /// `(table, column)` pairs the generator wrote explicitly. Every other column holds a
    /// migration or `DEFAULT` value.
    pub explicit_columns: BTreeSet<(String, String)>,
}

/// Writes `spec` to `out_root/<name>`.
///
/// An existing case directory is frozen evidence: it is kept unless `replace` is set. The case is
/// built in a staging directory beside the target and renamed into place only when complete, so
/// a failure never leaves a half-written fixture.
pub fn write_case(repo: &Path, out_root: &Path, spec: &CaseSpec, replace: bool) -> Result<Outcome> {
    let target = out_root.join(&spec.name);
    if fs::symlink_metadata(&target).is_ok() && !replace {
        return Ok(Outcome::KeptExisting(target));
    }
    fs::create_dir_all(out_root)?;
    let staging = out_root.join(format!(".{}.xtask-staging", spec.name));
    if fs::symlink_metadata(&staging).is_ok() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(staging.join("artifacts"))?;
    let built = match build_case(repo, &staging, spec) {
        Ok(built) => built,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(format!("{}: {error}", spec.name).into());
        }
    };
    if fs::symlink_metadata(&target).is_ok() {
        fs::remove_dir_all(&target)?;
    }
    fs::rename(&staging, &target)?;
    Ok(Outcome::Wrote(Box::new(Generated {
        dir: target,
        manifest: built.0,
        explicit_columns: built.1,
    })))
}

/// SHA-256 of `bytes` as lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes.as_ref()))
}

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

/// A JavaScript number. better-sqlite3 binds every JS number with `sqlite3_bind_double`, and the
/// column affinity then decides the stored type, so the port binds a REAL too.
fn number(value: usize) -> Value {
    Value::Real(f64::from(
        u32::try_from(value).expect("fixture numbers are small"),
    ))
}

/// Port of the Node `insert()` helper: skip a table that does not exist yet and keep only the
/// columns that exist at this schema.
struct Rows<'conn> {
    conn: &'conn Connection,
    explicit: BTreeSet<(String, String)>,
}

impl Rows<'_> {
    fn insert(&mut self, table: &str, values: &[(&str, Value)]) -> Result<()> {
        if !table_exists(self.conn, table)? {
            return Ok(());
        }
        let available = columns(self.conn, table)?;
        let entries = values
            .iter()
            .filter(|(column, _)| available.contains(*column))
            .collect::<Vec<_>>();
        let names = entries
            .iter()
            .map(|(column, _)| *column)
            .collect::<Vec<_>>();
        let placeholders = (1..=entries.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>();
        let sql = format!(
            "INSERT INTO {table} ({}) VALUES ({})",
            names.join(", "),
            placeholders.join(", ")
        );
        self.conn.execute(
            &sql,
            rusqlite::params_from_iter(entries.iter().map(|(_, value)| value)),
        )?;
        for name in names {
            self.explicit.insert((table.to_owned(), name.to_owned()));
        }
        Ok(())
    }

    fn artifact(&mut self, id: &str, title: &str, options: &ArtifactOptions) -> Result<()> {
        self.insert(
            "artifacts",
            &[
                ("id", text(id)),
                ("client_id", text("fixture-key")),
                ("org", text("fixture")),
                ("title", text(title)),
                ("description", text(FIXTURE_DESCRIPTION)),
                ("bytes", number(options.bytes)),
                ("uploader_label", text("Fixture key")),
                ("is_bundle", number(usize::from(options.is_bundle))),
                ("entry", text(options.entry)),
                ("revision", number(options.revision)),
                ("category", text("fixtures")),
                ("hidden", number(0)),
                ("body_sha256", text(&options.digest)),
                ("owner_email", text(FIXTURE_OWNER)),
            ],
        )
    }

    fn revision(
        &mut self,
        artifact_id: &str,
        title: &str,
        bytes: usize,
        digest: &str,
    ) -> Result<()> {
        self.insert(
            "artifact_revisions",
            &[
                ("artifact_id", text(artifact_id)),
                ("org", text("fixture")),
                ("revision", number(1)),
                ("title", text(title)),
                ("description", text(FIXTURE_DESCRIPTION)),
                ("category", text("fixtures")),
                ("bytes", number(bytes)),
                ("is_bundle", number(0)),
                ("entry", text("")),
                ("body_sha256", text(digest)),
                ("client_id", text("fixture-key")),
            ],
        )
    }

    fn intent(&mut self, values: [&str; 7]) -> Result<()> {
        let [id, artifact_id, operation, state, expected, prior, staging] = values;
        self.insert(
            "artifact_durability_intents",
            &[
                ("id", text(id)),
                ("artifact_id", text(artifact_id)),
                ("operation", text(operation)),
                ("state", text(state)),
                ("expected_sha256", text(expected)),
                ("prior_sha256", text(prior)),
                ("staging_path", text(staging)),
            ],
        )
    }
}

/// Options of the Node `artifact()` helper, with its defaults.
struct ArtifactOptions {
    bytes: usize,
    is_bundle: bool,
    entry: &'static str,
    revision: usize,
    digest: String,
}

impl Default for ArtifactOptions {
    fn default() -> Self {
        Self {
            bytes: 0,
            is_bundle: false,
            entry: "",
            revision: 1,
            digest: String::new(),
        }
    }
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn columns(conn: &Connection, table: &str) -> Result<BTreeSet<String>> {
    let mut statement = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
    let names = statement
        .query_map([table], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    Ok(names)
}

/// Port of the Node `encrypted()` helper: AES-256-GCM under [`FIXTURE_KEY`] with a nonce of
/// twelve `nonce_byte` bytes, plus the masked display URL.
fn encrypted(url: &str, nonce_byte: u8) -> Result<Vec<(&'static str, Value)>> {
    let key = BASE64.decode(FIXTURE_KEY)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| "fixture key is not 32 bytes")?;
    let nonce = [nonce_byte; 12];
    let mut ciphertext = url.as_bytes().to_vec();
    let tag = cipher
        .encrypt_in_place_detached(&Nonce::from(nonce), &[], &mut ciphertext)
        .map_err(|_| "AES-GCM refused the fixture URL")?;
    // `url.slice(-4)`: the fixture URLs are ASCII, so four chars are four UTF-16 code units.
    let tail = url
        .char_indices()
        .rev()
        .nth(3)
        .map_or(url, |(index, _)| &url[index..]);
    Ok(vec![
        ("url", Value::Text(format!("https://discord.com/…{tail}"))),
        ("url_cipher", Value::Text(BASE64.encode(&ciphertext))),
        ("url_nonce", Value::Text(BASE64.encode(nonce))),
        ("url_tag", Value::Text(BASE64.encode(tag))),
    ])
}

fn write_body(root: &Path, relative: &str, contents: &str) -> Result<()> {
    let full = root.join("artifacts").join(relative);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(full, contents)?;
    Ok(())
}

/// Every file under `<root>/artifacts`, with `/` separators. The Node generator sorted with
/// `localeCompare`; for the fixture file names this is the same as byte order.
pub fn body_manifest(root: &Path) -> Result<Vec<BodyEntry>> {
    fn visit(base: &Path, dir: &Path, out: &mut Vec<BodyEntry>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                visit(base, &path, out)?;
            } else if kind.is_file() {
                let bytes = fs::read(&path)?;
                let relative = path
                    .strip_prefix(base)?
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push(BodyEntry {
                    path: relative,
                    bytes: u64::try_from(bytes.len())?,
                    sha256: sha256_hex(&bytes),
                });
            }
        }
        Ok(())
    }
    let base = root.join("artifacts");
    let mut entries = Vec::new();
    visit(&base, &base, &mut entries)?;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

/// Port of `buildCase` for the current-ledger cases.
fn build_case(
    repo: &Path,
    root: &Path,
    spec: &CaseSpec,
) -> Result<(Manifest, BTreeSet<(String, String)>)> {
    let db_path = root.join("artifacts.db");
    let mut conn = Connection::open(&db_path)?;
    conn.execute_batch("PRAGMA foreign_keys = ON")?;
    if spec.origin_schema == 0 {
        // A real pre-ledger state: version 1's CREATE TABLE IF NOT EXISTS cannot repair these
        // old layouts, so later migrations must evolve them as a long-lived deployment would.
        conn.execute_batch(PRE_LEDGER_LAYOUT)?;
    } else {
        // No ORG_EMAIL_DOMAINS seeding: a fixture must not depend on the operator's environment.
        migrations::migrate_through(&mut conn, &MigrationContext::empty(), spec.origin_schema)?;
    }
    let version = if table_exists(&conn, "schema_migrations")? {
        conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get::<_, Option<i64>>(0)
        })?
        .unwrap_or(0)
    } else {
        0
    };
    if version != spec.origin_schema {
        return Err(format!(
            "{} produced schema {version}, expected {}",
            spec.name, spec.origin_schema
        )
        .into());
    }

    let mut rows = Rows {
        conn: &conn,
        explicit: BTreeSet::new(),
    };
    seed_rows(&mut rows, spec)?;
    let explicit = rows.explicit;
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    conn.close().map_err(|(_, error)| error)?;

    write_bodies(root, spec)?;

    let durability = spec.durability_recovery;
    let expected_recovery = if spec.recovery {
        ExpectedRecovery {
            recovered_paths: strings(if durability {
                &[
                    ".intentpub24.staging-fixture",
                    ".intentup24.staging-fixture",
                    ".recover23.staging-fixture",
                    ".intentdel24.trash-fixture",
                    ".trash23.trash-fixture",
                ]
            } else {
                &[".recover23.staging-fixture", ".trash23.trash-fixture"]
            }),
            divergent_bodies: strings(if durability {
                &["diverge23", "ambigup24"]
            } else {
                &["diverge23"]
            }),
            orphan_bodies: strings(&["orphan23.html"]),
            missing_bodies: strings(&["missing23"]),
            preserved_transient_paths: strings(if durability {
                &[".ambigup24.staging-fixture", ".diverge23.staging-fixture"]
            } else {
                &[".diverge23.staging-fixture"]
            }),
            remaining_intent_ids: Some(strings(if durability {
                &["update:ambigup24:2"]
            } else {
                &[]
            })),
            // The Node generator wrote `null` for a recovery case without durability state;
            // only the tag-built release-v23 fixture has that shape and it is never regenerated.
            prepared_metadata_only: durability.then(|| PreparedMetadataOnly {
                id: "prepared24".to_owned(),
                revision: 2,
                history_revision: 1,
                html: "<main>legacy prepared metadata body</main>".to_owned(),
            }),
        }
    } else {
        ExpectedRecovery {
            recovered_paths: Vec::new(),
            divergent_bodies: Vec::new(),
            orphan_bodies: Vec::new(),
            missing_bodies: Vec::new(),
            preserved_transient_paths: Vec::new(),
            remaining_intent_ids: Some(Vec::new()),
            prepared_metadata_only: None,
        }
    };

    let manifest = Manifest {
        schema_version: 1,
        synthetic: true,
        origin: Origin {
            release_tag: None,
            schema_version: spec.origin_schema,
            kind: spec.kind().to_owned(),
            source_ref: SourceRef {
                reference: LEDGER_REF.to_owned(),
                migration_ledger_sha256: ledger_digest(repo)?,
            },
        },
        // Derived from the compiled production ledger, never a copied literal.
        expected_target_schema: LATEST_SCHEMA_VERSION,
        generation: GENERATION.to_owned(),
        webhook_encryption: (spec.webhook_mode == WebhookMode::Mixed).then(|| WebhookEncryption {
            key_id: "fixture-zero-key-v1".to_owned(),
            key: FIXTURE_KEY.to_owned(),
        }),
        authentication: Authentication {
            client_id: "fixture-key".to_owned(),
            token: FIXTURE_API_TOKEN.to_owned(),
            classification: "public synthetic test material".to_owned(),
        },
        expected_recovery,
        database: DatabaseRef {
            path: "artifacts.db".to_owned(),
            sha256: sha256_hex(fs::read(&db_path)?),
        },
        bodies: body_manifest(root)?,
    };
    fs::write(root.join("fixture.json"), manifest.to_file_bytes()?)?;
    Ok((manifest, explicit))
}

/// SHA-256 of the migration ledger source the compiled generator embeds.
pub fn ledger_digest(repo: &Path) -> Result<String> {
    Ok(sha256_hex(fs::read(repo.join(LEDGER_SOURCE))?))
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn suffix(spec: &CaseSpec) -> String {
    if spec.boundary {
        format!("b{:02}", spec.origin_schema)
    } else {
        "16".to_owned()
    }
}

/// The synthetic rows, in the Node generator's order.
fn seed_rows(rows: &mut Rows<'_>, spec: &CaseSpec) -> Result<()> {
    rows.insert(
        "orgs",
        &[
            ("name", text("fixture")),
            ("label", text("Synthetic fixture")),
            ("color", text("#3569a8")),
        ],
    )?;
    // A real empty tenant exercises migrations that add organization-scoped tables without any
    // keys, artifacts, webhooks, or categories to seed them accidentally.
    rows.insert(
        "orgs",
        &[
            ("name", text("emptyfixture")),
            ("label", text("Synthetic empty organization")),
        ],
    )?;
    rows.insert(
        "org_categories",
        &[("org", text("fixture")), ("name", text("fixtures"))],
    )?;
    rows.insert(
        "api_keys",
        &[
            ("client_id", text("fixture-key")),
            ("key_hash", text(&sha256_hex(FIXTURE_API_TOKEN))),
            ("org", text("fixture")),
            ("label", text("Fixture key")),
            ("role", text("author")),
            ("owner_email", text(FIXTURE_OWNER)),
        ],
    )?;

    let suffix = suffix(spec);
    let single_id = format!("single{suffix}");
    let bundle_id = format!("bundle{suffix}");
    let has_revisions = table_exists(rows.conn, "artifact_revisions")?;
    let has_feedback = table_exists(rows.conn, "feedback")?;
    rows.artifact(
        &single_id,
        "Historical single-file artifact",
        &ArtifactOptions {
            bytes: CURRENT_SINGLE.len(),
            revision: if has_revisions { 2 } else { 1 },
            ..ArtifactOptions::default()
        },
    )?;
    if !spec.boundary {
        rows.artifact(
            &bundle_id,
            "Historical bundle artifact",
            &ArtifactOptions {
                bytes: BUNDLE_INDEX.len() + BUNDLE_CSS.len(),
                is_bundle: true,
                entry: "index.html",
                ..ArtifactOptions::default()
            },
        )?;
    }
    if has_revisions {
        rows.revision(
            &single_id,
            "Historical single-file artifact",
            HISTORICAL_SINGLE.len(),
            "",
        )?;
    }
    if has_feedback {
        rows.insert(
            "feedback",
            &[
                ("id", text(&format!("feedback{suffix}"))),
                ("artifact_id", text(&single_id)),
                ("org", text("fixture")),
                ("viewer_email", text("viewer@example.test")),
                ("body", text("Synthetic unresolved feedback")),
                ("artifact_revision", number(2)),
            ],
        )?;
    }
    if !spec.boundary && has_feedback {
        rows.insert(
            "feedback",
            &[
                ("id", text(&format!("resolved{suffix}"))),
                ("artifact_id", text(&single_id)),
                ("org", text("fixture")),
                ("viewer_email", text("reviewer@example.test")),
                ("body", text("Synthetic resolved feedback")),
                ("artifact_revision", number(1)),
                ("resolved_at", text("2026-01-02 00:00:00")),
                ("resolved_by", text(FIXTURE_OWNER)),
            ],
        )?;
    }
    if !spec.boundary && table_exists(rows.conn, "artifact_shares")? {
        let share = |token: &str| {
            vec![
                ("token", text(token)),
                ("artifact_id", text(&single_id)),
                ("org", text("fixture")),
                ("created_by", text("fixture-key")),
            ]
        };
        rows.insert("artifact_shares", &share("aaaaaaaaaaaaaaaaaaaaaaaa"))?;
        let mut expired = share("bbbbbbbbbbbbbbbbbbbbbbbb");
        expired.push(("expires_at", text("2000-01-01 00:00:00")));
        rows.insert("artifact_shares", &expired)?;
        let mut revoked = share("cccccccccccccccccccccccc");
        revoked.push(("revoked_at", text("2026-01-01 00:00:00")));
        rows.insert("artifact_shares", &revoked)?;
    }
    if spec.origin_schema >= 21 {
        rows.insert(
            "org_email_members",
            &[
                ("email", text("member@example.test")),
                ("org", text("fixture")),
                ("display_name", text("Fixture Reader")),
            ],
        )?;
    }
    if spec.origin_schema >= 34 {
        for (scope, viewer, value, revision) in [
            ("org", "", "\"Shared fixture note\"", 4),
            (
                "viewer",
                "member@example.test",
                "\"Private fixture note\"",
                2,
            ),
        ] {
            rows.insert(
                "artifact_state",
                &[
                    ("artifact_id", text(&single_id)),
                    ("key", text("note")),
                    ("updated_at", text("2026-01-02 00:00:00")),
                    ("updated_by", text("member@example.test")),
                    ("scope", text(scope)),
                    ("viewer", text(viewer)),
                    ("value", text(value)),
                    ("revision", number(revision)),
                ],
            )?;
        }
    }
    if spec.webhook_mode == WebhookMode::Mixed {
        rows.insert(
            "org_webhooks",
            &[
                ("id", text("plainwh20")),
                ("org", text("fixture")),
                (
                    "url",
                    text("https://discord.com/api/webhooks/1000/synthetic-plaintext-token"),
                ),
                ("label", text("Synthetic plaintext")),
                ("events", text("published,feedback")),
            ],
        )?;
        let mut encrypted_row = vec![
            ("id", text("encwh200")),
            ("org", text("fixture")),
            ("label", text("Synthetic encrypted")),
            ("events", text("updated,resolved")),
        ];
        encrypted_row.extend(encrypted(
            "https://discord.com/api/webhooks/2000/synthetic-encrypted-token",
            20,
        )?);
        rows.insert("org_webhooks", &encrypted_row)?;
    }
    if spec.recovery {
        seed_recovery_rows(rows)?;
    }
    if spec.durability_recovery {
        seed_durability_rows(rows)?;
    }
    Ok(())
}

fn seed_recovery_rows(rows: &mut Rows<'_>) -> Result<()> {
    let staged = "<main>recovered staged revision</main>";
    rows.artifact(
        "recover23",
        "Recover staged update",
        &ArtifactOptions {
            bytes: staged.len(),
            revision: 2,
            digest: sha256_hex(staged),
            ..ArtifactOptions::default()
        },
    )?;
    rows.revision(
        "recover23",
        "Recover staged update",
        27,
        &sha256_hex("<main>outgoing revision</main>"),
    )?;
    rows.artifact(
        "diverge23",
        "Quarantined divergence",
        &ArtifactOptions {
            bytes: 30,
            digest: sha256_hex("<main>expected but absent</main>"),
            ..ArtifactOptions::default()
        },
    )?;
    rows.artifact(
        "missing23",
        "Missing body report",
        &ArtifactOptions {
            digest: sha256_hex("missing"),
            ..ArtifactOptions::default()
        },
    )?;
    let trashed = "<main>interrupted delete body</main>";
    rows.artifact(
        "trash23",
        "Recover interrupted delete",
        &ArtifactOptions {
            bytes: trashed.len(),
            digest: sha256_hex(trashed),
            ..ArtifactOptions::default()
        },
    )
}

fn seed_durability_rows(rows: &mut Rows<'_>) -> Result<()> {
    let next = "<main>durability committed update</main>";
    let prior = "<main>durability outgoing update</main>";
    rows.artifact(
        "intentup24",
        "Intent update",
        &ArtifactOptions {
            bytes: next.len(),
            revision: 2,
            digest: sha256_hex(next),
            ..ArtifactOptions::default()
        },
    )?;
    rows.revision(
        "intentup24",
        "Intent update",
        prior.len(),
        &sha256_hex(prior),
    )?;
    // Former commit-to-intent-marker gap: an old binary has committed the metadata and revision
    // but left the marker `prepared`. Recovery must infer target revision 2, build the immutable
    // predecessor body, then release the marker.
    let prepared_body = "<main>legacy prepared metadata body</main>";
    rows.artifact(
        "prepared24",
        "Prepared metadata recovery",
        &ArtifactOptions {
            bytes: prepared_body.len(),
            revision: 2,
            digest: sha256_hex(prepared_body),
            ..ArtifactOptions::default()
        },
    )?;
    rows.revision(
        "prepared24",
        "Prepared metadata recovery",
        prepared_body.len(),
        &sha256_hex(prepared_body),
    )?;
    let expected_intact = "<main>expected intact body</main>";
    rows.artifact(
        "ambigup24",
        "Ambiguous truncated staging",
        &ArtifactOptions {
            bytes: 31,
            digest: sha256_hex(expected_intact),
            ..ArtifactOptions::default()
        },
    )?;
    let published = "<main>published before final install</main>";
    let deleted = "<main>delete interrupted before SQL</main>";
    rows.artifact(
        "intentpub24",
        "Intent publish",
        &ArtifactOptions {
            bytes: published.len(),
            digest: sha256_hex(published),
            ..ArtifactOptions::default()
        },
    )?;
    rows.artifact(
        "intentdel24",
        "Intent delete",
        &ArtifactOptions {
            bytes: deleted.len(),
            digest: sha256_hex(deleted),
            ..ArtifactOptions::default()
        },
    )?;
    let digest = |value: &str| sha256_hex(value);
    rows.intent([
        "update:intentup24:2",
        "intentup24",
        "update",
        "metadata_committed",
        &digest(next),
        &digest(prior),
        ".intentup24.staging-fixture",
    ])?;
    rows.intent([
        "update:prepared24:2",
        "prepared24",
        "update",
        "prepared",
        &digest(prepared_body),
        &digest(prepared_body),
        ".prepared24.staging-fixture",
    ])?;
    rows.intent([
        "update:ambigup24:2",
        "ambigup24",
        "update",
        "prepared",
        &digest(expected_intact),
        "",
        ".ambigup24.staging-fixture",
    ])?;
    rows.intent([
        "delete:gone24:1",
        "gone24",
        "delete",
        "metadata_committed",
        "",
        &digest("<main>deleted durable trash</main>"),
        ".gone24.trash-fixture",
    ])?;
    rows.intent([
        "publish:norow24",
        "norow24",
        "publish",
        "prepared",
        &digest("<main>uncommitted publish</main>"),
        "",
        ".norow24.staging-fixture",
    ])?;
    rows.intent([
        "publish:intentpub24",
        "intentpub24",
        "publish",
        "metadata_committed",
        &digest(published),
        "",
        ".intentpub24.staging-fixture",
    ])?;
    rows.intent([
        "delete:intentdel24:1",
        "intentdel24",
        "delete",
        "prepared",
        "",
        &digest(deleted),
        ".intentdel24.trash-fixture",
    ])
}

/// The body files, in the Node generator's order.
fn write_bodies(root: &Path, spec: &CaseSpec) -> Result<()> {
    let suffix = suffix(spec);
    write_body(root, &format!("single{suffix}.html"), CURRENT_SINGLE)?;
    if !spec.boundary {
        write_body(root, &format!("bundle{suffix}/index.html"), BUNDLE_INDEX)?;
        write_body(root, &format!("bundle{suffix}/assets/site.css"), BUNDLE_CSS)?;
        write_body(
            root,
            &format!(".history/single{suffix}/1.html"),
            HISTORICAL_SINGLE,
        )?;
    }
    if spec.recovery {
        for (path, body) in [
            ("recover23.html", "<main>outgoing revision</main>"),
            (
                ".recover23.staging-fixture",
                "<main>recovered staged revision</main>",
            ),
            ("diverge23.html", "<main>unexpected installed body</main>"),
            (
                ".diverge23.staging-fixture",
                "<main>untrusted staged body</main>",
            ),
            (
                ".trash23.trash-fixture",
                "<main>interrupted delete body</main>",
            ),
            ("orphan23.html", "<main>orphan body is reported</main>"),
        ] {
            write_body(root, path, body)?;
        }
    }
    if spec.durability_recovery {
        for (path, body) in [
            ("intentup24.html", "<main>durability outgoing update</main>"),
            (
                "prepared24.html",
                "<main>legacy prepared metadata body</main>",
            ),
            (
                ".intentup24.staging-fixture",
                "<main>durability committed update</main>",
            ),
            ("ambigup24.html", "<main>installed but not expected</main>"),
            (".ambigup24.staging-fixture", "<main>truncated"),
            (
                ".gone24.trash-fixture",
                "<main>deleted durable trash</main>",
            ),
            (
                ".norow24.staging-fixture",
                "<main>uncommitted publish</main>",
            ),
            (
                ".intentpub24.staging-fixture",
                "<main>published before final install</main>",
            ),
            (
                ".intentdel24.trash-fixture",
                "<main>delete interrupted before SQL</main>",
            ),
        ] {
            write_body(root, path, body)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_names_and_ids_match_the_node_generator() {
        assert_eq!(boundary_name(0), "boundary-v00");
        assert_eq!(boundary_name(41), "boundary-v41");
        assert_eq!(suffix(&CaseSpec::boundary(7)), "b07");
        assert_eq!(suffix(&CaseSpec::durability_recovery()), "16");
        assert_eq!(
            generatable_cases().len(),
            usize::try_from(LATEST_SCHEMA_VERSION).expect("schema fits usize") + 2
        );
        assert!(case_by_name("release-v16").is_err());
        assert!(case_by_name("boundary-v00").is_ok());
    }

    #[test]
    fn encrypted_webhook_shape_matches_the_node_generator() {
        let row = encrypted(
            "https://discord.com/api/webhooks/2000/synthetic-encrypted-token",
            20,
        )
        .expect("encrypt");
        let names = row.iter().map(|(name, _)| *name).collect::<Vec<_>>();
        assert_eq!(names, ["url", "url_cipher", "url_nonce", "url_tag"]);
        assert_eq!(row[0].1, text("https://discord.com/…oken"));
        assert_eq!(row[2].1, text(&BASE64.encode([20_u8; 12])));
    }
}
