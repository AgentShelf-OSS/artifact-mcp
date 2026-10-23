//! PBI-050 frozen migration/recovery corpus.
//!
//! The fixtures are copied before opening them. This is deliberately a full production bootstrap
//! (`Database::open_with` then storage reconciliation then digest backfill), rather than a unit
//! test that calls a migration function in isolation.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use artifact_mcp::artifacts::{digest::body_digest_at_path, lifecycle::ArtifactStore};
use artifact_mcp::config::{Secret, SequentialIdSource, StorageLimits, SystemClock};
use artifact_mcp::model::{
    ArtifactContent, ArtifactId, ArtifactUpdate, CreateShare, EmailAddress, OrgId, PublishArtifact,
    PublisherIdentity, Viewer, WebhookId,
};
use artifact_mcp::persistence::{
    db::{self, Database},
    migrations::{self, MigrationContext},
    webhooks::WebhookStore,
};
use artifact_mcp::ports::ArtifactService;
use artifact_mcp::security::{
    access::AccessPolicy,
    audit::{MutationAudit, parse_hmac_key},
    crypto::{WebhookCipher, WebhookUrlProtection},
};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::u03_support::TempDataDir;

const FIXTURE_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

fn fixture_mutation_audit() -> MutationAudit {
    MutationAudit::publisher(&PublisherIdentity {
        client_id: "fixture-key".into(),
        org: "fixture".into(),
        label: "Fixture key".to_owned(),
        role: "author".to_owned(),
        scopes: None,
    })
    .expect("fixture audit context")
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("conformance/fixtures/historical")
}

fn case_names() -> Vec<String> {
    let mut names = std::fs::read_dir(fixture_root())
        .expect("read fixture root")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("create copied fixture root");
    for entry in std::fs::read_dir(source).expect("read fixture source") {
        let entry = entry.expect("fixture entry");
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if entry.file_type().expect("fixture type").is_dir() {
            copy_tree(&from, &to);
        } else {
            std::fs::copy(&from, &to).expect("copy immutable fixture file");
        }
    }
}

fn source_digest(path: &Path) -> String {
    artifact_mcp::artifacts::digest::sha256_hex(&std::fs::read(path).expect("read source database"))
}

fn values(manifest: &Value, key: &str) -> Vec<String> {
    manifest["expectedRecovery"][key]
        .as_array()
        .expect("expected recovery array")
        .iter()
        .map(|value| value.as_str().expect("recovery string").to_owned())
        .collect()
}

fn optional_values(manifest: &Value, key: &str) -> Vec<String> {
    manifest["expectedRecovery"][key]
        .as_array()
        .map(|values| {
            values
                .iter()
                .map(|value| value.as_str().expect("recovery string").to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn file_bytes(path: &Path) -> u64 {
    if path.is_file() {
        return std::fs::metadata(path).expect("file metadata").len();
    }
    std::fs::read_dir(path)
        .expect("bundle directory")
        .map(|entry| {
            let path = entry.expect("bundle entry").path();
            file_bytes(&path)
        })
        .sum()
}

/// Every regular file under `root`, as (`/`-separated relative path, bytes, SHA-256), sorted by
/// path. Symbolic links and other special files are not fixture content and are skipped.
fn file_manifest(root: &Path) -> Vec<(String, u64, String)> {
    fn visit(base: &Path, directory: &Path, entries: &mut Vec<(String, u64, String)>) {
        for entry in std::fs::read_dir(directory).expect("read fixture directory") {
            let entry = entry.expect("fixture directory entry");
            let kind = entry.file_type().expect("fixture entry type");
            let path = entry.path();
            if kind.is_dir() {
                visit(base, &path, entries);
            } else if kind.is_file() {
                let bytes = std::fs::read(&path).expect("read fixture file");
                let relative = path
                    .strip_prefix(base)
                    .expect("fixture file under its root")
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                entries.push((
                    relative,
                    bytes.len() as u64,
                    artifact_mcp::artifacts::digest::sha256_hex(&bytes),
                ));
            }
        }
    }
    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort();
    entries
}

/// SQLite side files that a read-only verification must never leave behind.
const SQLITE_SIDE_FILES: [&str; 3] = ["-shm", "-wal", "-journal"];

/// Verify a frozen fixture before anything opens it for writing (formerly
/// `scripts/verify-historical-fixtures.mjs`): the manifest is a synthetic schema-1 manifest, the
/// database bytes match its digest, the body manifest lists exactly the bytes and SHA-256 of every
/// file under `artifacts/`, the database passes `PRAGMA integrity_check` read-only, and that
/// read-only check leaves no SQLite side file next to the source.
fn verify_frozen_fixture(root: &Path) -> Result<(), String> {
    let name = root
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(root.join("fixture.json"))
            .map_err(|error| format!("{name}: read fixture.json: {error}"))?,
    )
    .map_err(|error| format!("{name}: fixture.json: {error}"))?;
    if manifest["synthetic"] != Value::Bool(true) || manifest["schemaVersion"].as_f64() != Some(1.0)
    {
        return Err(format!("{name}: invalid fixture manifest"));
    }
    let database_name = manifest["database"]["path"]
        .as_str()
        .ok_or_else(|| format!("{name}: manifest names no database"))?;
    let database = root.join(database_name);
    let digest = artifact_mcp::artifacts::digest::sha256_hex(
        &std::fs::read(&database).map_err(|error| format!("{name}: read database: {error}"))?,
    );
    if Some(digest.as_str()) != manifest["database"]["sha256"].as_str() {
        return Err(format!("{name}: artifacts.db digest changed"));
    }

    // Order is not part of the contract (the generator sorted with a locale collation), so both
    // sides are compared sorted by path; every entry must carry exactly path, bytes and sha256.
    let mut recorded = Vec::new();
    for entry in manifest["bodies"]
        .as_array()
        .ok_or_else(|| format!("{name}: manifest has no body list"))?
    {
        let fields = entry.as_object().map(|fields| {
            let mut keys = fields.keys().map(String::as_str).collect::<Vec<_>>();
            keys.sort_unstable();
            keys
        });
        let parsed = (
            entry["path"].as_str(),
            entry["bytes"].as_u64(),
            entry["sha256"].as_str(),
        );
        match (fields.as_deref(), parsed) {
            (Some(["bytes", "path", "sha256"]), (Some(path), Some(bytes), Some(sha256))) => {
                recorded.push((path.to_owned(), bytes, sha256.to_owned()));
            }
            _ => return Err(format!("{name}: malformed body manifest entry {entry}")),
        }
    }
    recorded.sort();
    let artifacts = root.join("artifacts");
    let actual = if artifacts.is_dir() {
        file_manifest(&artifacts)
    } else {
        Vec::new()
    };
    if actual != recorded {
        let changed = actual
            .iter()
            .filter(|entry| !recorded.contains(entry))
            .chain(recorded.iter().filter(|entry| !actual.contains(entry)))
            .map(|(path, _, _)| path.as_str())
            .collect::<Vec<_>>();
        return Err(format!("{name}: body manifest changed: {changed:?}"));
    }

    // Integrity before the application touches a copy catches bad fixture snapshots instead of
    // allowing migration to mask the defect.
    let integrity = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|raw| raw.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0)))
        .map_err(|error| format!("{name}: pre-migration integrity_check failed: {error}"))?;
    if integrity != "ok" {
        return Err(format!(
            "{name}: pre-migration integrity_check is {integrity}"
        ));
    }
    for suffix in SQLITE_SIDE_FILES {
        let side_file = root.join(format!("{database_name}{suffix}"));
        if side_file.exists() {
            return Err(format!(
                "{name}: read-only verification left {}",
                side_file.display()
            ));
        }
    }
    Ok(())
}

#[test]
fn the_frozen_fixture_verifier_rejects_changed_bytes_flags_and_side_files() {
    let (name, manifest) = case_names()
        .into_iter()
        .find_map(|name| {
            let manifest: Value = serde_json::from_slice(
                &std::fs::read(fixture_root().join(&name).join("fixture.json"))
                    .expect("read fixture manifest"),
            )
            .expect("fixture manifest JSON");
            manifest["bodies"]
                .as_array()
                .is_some_and(|bodies| !bodies.is_empty())
                .then_some((name, manifest))
        })
        .expect("a frozen fixture with artifact bodies");
    let body = manifest["bodies"][0]["path"]
        .as_str()
        .expect("body path")
        .to_owned();
    let copy = |label: &str| {
        let copied = TempDataDir::new(&format!("historical-verify-{label}"));
        copy_tree(&fixture_root().join(&name), copied.path());
        copied
    };
    let flip_byte = |path: &Path| {
        let mut bytes = std::fs::read(path).expect("read copied file");
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x01;
        std::fs::write(path, bytes).expect("write copied file");
    };
    let rejected = |copied: &TempDataDir, expected: &str| {
        let error = verify_frozen_fixture(copied.path()).expect_err("verification must fail");
        assert!(error.contains(expected), "{expected:?} not in {error:?}");
    };

    let faithful = copy("faithful");
    verify_frozen_fixture(faithful.path()).expect("an exact copy verifies");

    let body_byte = copy("body-byte");
    flip_byte(&body_byte.path().join("artifacts").join(&body));
    rejected(&body_byte, "body manifest changed");

    let extra_body = copy("extra-body");
    std::fs::write(
        extra_body.path().join("artifacts/unlisted.html"),
        "<p>x</p>",
    )
    .expect("add unlisted body");
    rejected(&extra_body, "body manifest changed");

    let database_byte = copy("database-byte");
    flip_byte(&database_byte.path().join("artifacts.db"));
    rejected(&database_byte, "artifacts.db digest changed");

    let not_synthetic = copy("not-synthetic");
    let manifest_path = not_synthetic.path().join("fixture.json");
    let mut edited: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).expect("read copied manifest"))
            .expect("copied manifest JSON");
    edited["synthetic"] = Value::Bool(false);
    std::fs::write(&manifest_path, edited.to_string()).expect("write copied manifest");
    rejected(&not_synthetic, "invalid fixture manifest");

    let side_file = copy("side-file");
    std::fs::write(side_file.path().join("artifacts.db-wal"), b"").expect("add stray WAL");
    rejected(&side_file, "read-only verification left");
}

#[tokio::test]
async fn every_frozen_historical_database_boots_recovers_and_serves_without_mutating_the_source() {
    let names = case_names();
    assert_eq!(
        names.len(),
        usize::try_from(migrations::LATEST_SCHEMA_VERSION).expect("schema fits usize") + 6,
        "every migration boundary plus four public-release fixtures"
    );

    for name in names {
        let source = fixture_root().join(&name);
        let source_tree = file_manifest(&source);
        verify_frozen_fixture(&source).unwrap_or_else(|error| panic!("{error}"));
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(source.join("fixture.json")).expect("read fixture manifest"),
        )
        .expect("fixture manifest JSON");
        // Fixtures are immutable source databases. A fixture made at an earlier release names
        // that release's then-current target, while the assertion below verifies that opening it
        // actually advances to today's append-only ledger.
        assert!(
            manifest["expectedTargetSchema"]
                .as_i64()
                .is_some_and(|version| version <= migrations::LATEST_SCHEMA_VERSION)
        );
        assert!(
            manifest["origin"]["sourceRef"].is_object(),
            "{name}: immutable source ref"
        );
        // `verify_frozen_fixture` already proved the digest and read-only integrity.
        let original_db = source.join("artifacts.db");
        let before = source_digest(&original_db);

        let copied = TempDataDir::new(&format!("historical-{name}"));
        copy_tree(&source, copied.path());
        let key = Secret::new(FIXTURE_KEY);
        let cipher = WebhookCipher::new(&key).expect("synthetic fixture cipher");
        let pool = Database::open_with(copied.path(), &MigrationContext::empty(), Some(&cipher))
            .unwrap_or_else(|error| {
                panic!("{name}: production migration bootstrap failed: {error}")
            });
        let store = ArtifactStore::new_for_test(
            pool.clone(),
            copied.path().join("artifacts"),
            StorageLimits::default(),
            Arc::new(SequentialIdSource::default()),
            parse_hmac_key(FIXTURE_KEY).expect("fixture audit key"),
        );

        let report = store
            .audit_storage(true)
            .await
            .expect("production storage reconciliation");
        let mut recovered = report.recovered_paths;
        let mut expected_recovered = values(&manifest, "recoveredPaths");
        recovered.sort();
        expected_recovered.sort();
        assert_eq!(recovered, expected_recovered, "{name}: recovery result");
        assert_eq!(
            report.divergent_bodies,
            values(&manifest, "divergentBodies"),
            "{name}: divergence stays report-only"
        );
        assert_eq!(
            report.orphan_bodies,
            values(&manifest, "orphanBodies"),
            "{name}: orphan report"
        );
        assert_eq!(
            report.missing_bodies,
            values(&manifest, "missingBodies"),
            "{name}: missing report"
        );
        assert_eq!(
            report
                .transient_paths
                .into_iter()
                .filter(|value| values(&manifest, "preservedTransientPaths").contains(value))
                .collect::<Vec<_>>(),
            values(&manifest, "preservedTransientPaths"),
            "{name}: recoverable divergence is not deleted"
        );
        store
            .backfill_body_digests()
            .await
            .expect("production digest backfill");

        let (intent_ids, rows) = {
            let conn = db::checkout(&pool).expect("checkout migrated database");
            let intent_ids = conn
                .prepare("SELECT id FROM artifact_durability_intents ORDER BY id")
                .expect("prepare remaining intents")
                .query_map([], |row| row.get::<_, String>(0))
                .expect("query remaining intents")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("read remaining intents");
            assert_eq!(
                migrations::current_version(&conn).expect("schema version"),
                migrations::LATEST_SCHEMA_VERSION
            );
            if manifest["origin"]["schemaVersion"]
                .as_i64()
                .expect("origin schema")
                >= 7
            {
                assert_eq!(
                    conn.query_row(
                        "SELECT COUNT(*) FROM orgs WHERE name = 'emptyfixture'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .expect("empty organization query"),
                    1,
                    "{name}: an empty organization survives migration"
                );
            }
            assert_eq!(
                conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                    .expect("post integrity"),
                "ok"
            );
            let mut statement = conn
                .prepare("SELECT id, is_bundle, bytes, body_sha256 FROM artifacts ORDER BY id")
                .expect("artifact metadata");
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, i64>(2)? as u64,
                        row.get::<_, String>(3)?,
                    ))
                })
                .expect("artifact rows")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("collect artifact rows");
            (intent_ids, rows)
        };
        assert_eq!(
            intent_ids,
            optional_values(&manifest, "remainingIntentIds"),
            "{name}: only ambiguous intent remains"
        );
        if let Some(expected) = manifest["expectedRecovery"]["preparedMetadataOnly"].as_object() {
            let id = expected["id"].as_str().expect("prepared fixture id");
            let revision = expected["revision"]
                .as_u64()
                .expect("prepared fixture revision");
            let history_revision = expected["historyRevision"]
                .as_u64()
                .expect("prepared fixture history revision");
            let html = expected["html"].as_str().expect("prepared fixture html");
            let prepared = store
                .find_meta(&ArtifactId(id.to_owned()))
                .await
                .expect("find prepared metadata-only fixture")
                .expect("prepared metadata-only fixture becomes readable");
            assert_eq!(prepared.revision, revision);
            let history = store
                .read_revision_body_for(&prepared, history_revision, None)
                .await
                .expect("read reconstructed prepared history")
                .expect("prepared history exists");
            assert_eq!(history.content, html.as_bytes());
        }
        let divergence = values(&manifest, "divergentBodies");
        let missing = values(&manifest, "missingBodies");
        for (id, is_bundle, bytes, digest) in rows {
            if divergence.contains(&id) || missing.contains(&id) {
                continue;
            }
            let path = if is_bundle {
                copied.path().join("artifacts").join(&id)
            } else {
                copied.path().join("artifacts").join(format!("{id}.html"))
            };
            assert_eq!(
                bytes,
                file_bytes(&path),
                "{name}/{id}: metadata byte length"
            );
            assert_eq!(
                digest,
                body_digest_at_path(&path, is_bundle).expect("body digest"),
                "{name}/{id}: metadata digest"
            );
        }

        // Representative authorized read, new write, update and historical restore all execute
        // against the migrated copy rather than a fresh database.
        let existing = store
            .find_meta(&ArtifactId(if name.starts_with("boundary") {
                format!("singleb{}", &name[10..])
            } else {
                "single16".to_owned()
            }))
            .await
            .expect("find fixture artifact")
            .expect("fixture artifact exists");
        // v0's pre-ledger rows legitimately acquire the historical `default` tenant during the
        // v2/v7 path, so an administrator is the stable authenticated reader across every
        // supported boundary (the authorization gate itself remains exercised).
        let viewer = Viewer {
            email: Some(EmailAddress::from("fixture-owner@example.test")),
            org: Some(OrgId::from("admin")),
            is_admin: true,
        };
        let authorized = AccessPolicy::authorize_viewer(&viewer, Some(existing.clone()))
            .expect("fixture viewer authorized");
        assert!(
            store
                .read_body(&authorized)
                .await
                .expect("authorized body read")
                .is_some()
        );
        if name == "release-v23-recovery" {
            let owner = Viewer {
                email: Some(EmailAddress::from("fixture-owner@example.test")),
                org: Some(OrgId::from("fixture")),
                is_admin: false,
            };
            assert!(
                AccessPolicy::authorize_viewer(&owner, Some(existing.clone())).is_ok(),
                "v23 owner reads their artifact"
            );
            let non_owner = Viewer {
                email: Some(EmailAddress::from("other@example.test")),
                org: Some(OrgId::from("fixture")),
                is_admin: false,
            };
            // Reading is organization-wide for signed-in viewers; owner enforcement is a
            // mutation rule, so pin the corresponding management decision rather than claiming
            // that a legitimate member cannot view a published artifact.
            assert!(
                !AccessPolicy::viewer_can_manage_artifact(&non_owner, &existing),
                "v23 non-owner cannot manage artifact"
            );
            assert!(
                AccessPolicy::viewer_can_manage_artifact(&owner, &existing),
                "v23 owner can manage artifact"
            );
        }
        let publisher = PublisherIdentity {
            client_id: "fixture-key".into(),
            org: "fixture".into(),
            label: "Fixture key".to_owned(),
            role: "author".to_owned(),
            scopes: None,
        };
        let audit = MutationAudit::publisher(&publisher).expect("fixture audit context");
        let created = store
            .publish(
                PublishArtifact {
                    publisher,
                    target_org: "fixture".into(),
                    title: Some("Fixture migration write".to_owned()),
                    description: None,
                    category: Some("fixtures".to_owned()),
                    content: ArtifactContent::SingleHtml("<main>new write</main>".to_owned()),
                    artifact_key: None,
                },
                audit,
            )
            .await
            .expect("new write after migration")
            .meta;
        let updated = store
            .update_for(
                &created,
                ArtifactUpdate {
                    expected_revision: created.revision,
                    content: Some(ArtifactContent::SingleHtml(
                        "<main>updated write</main>".to_owned(),
                    )),
                    ..ArtifactUpdate::default()
                },
                fixture_mutation_audit(),
            )
            .await
            .expect("update after migration")
            .meta;
        assert_eq!(
            store
                .restore_for(
                    &updated,
                    created.revision,
                    Some("fixture-key".into()),
                    fixture_mutation_audit(),
                )
                .await
                .expect("restore history after migration")
                .restored_from,
            created.revision
        );

        // Shares are migration-sensitive (the composite FK arrived after the oldest boundary),
        // so verify a freshly-created share resolves, lists, and revokes on every migrated copy.
        {
            let conn = db::checkout(&pool).expect("checkout shares");
            let ids = SequentialIdSource::default();
            let share = artifact_mcp::persistence::shares::create(
                &conn,
                &ids,
                &SystemClock,
                &created.id,
                &created.org,
                &CreateShare {
                    created_by: "fixture-key".to_owned(),
                    expires: "never".to_owned(),
                },
            )
            .expect("create share after migration");
            assert!(
                artifact_mcp::persistence::shares::resolve(&conn, &share.token)
                    .expect("resolve new share")
                    .is_some()
            );
            assert!(
                artifact_mcp::persistence::shares::list_for_artifact(&conn, &created.id)
                    .expect("list new share")
                    .iter()
                    .any(|row| row.token == share.token)
            );
            assert!(
                artifact_mcp::persistence::shares::revoke(&conn, &created.id, &share.token)
                    .expect("revoke new share")
            );
            assert!(
                artifact_mcp::persistence::shares::resolve(&conn, &share.token)
                    .expect("resolve revoked share")
                    .is_none()
            );
        }

        if manifest["webhookEncryption"].is_object() {
            let protection =
                WebhookUrlProtection::from_config_key(Some(&key)).expect("fixture protection");
            let webhook_store = WebhookStore::new(
                pool.clone(),
                Arc::new(SequentialIdSource::default()),
                Arc::new(protection),
            );
            let delivery = webhook_store
                .delivery(&WebhookId::from("encwh200"))
                .await
                .expect("encrypted fixture delivery")
                .expect("encrypted webhook row");
            assert!(
                delivery.url.ends_with("synthetic-encrypted-token"),
                "{name}: encrypted webhook decrypts"
            );
        }
        assert_eq!(
            source_digest(&original_db),
            before,
            "{name}: checked-in database was never mutated"
        );
        // No file was added, removed or changed anywhere in the source fixture (this includes
        // SQLite -shm/-wal/-journal side files and every artifact body).
        assert_eq!(
            file_manifest(&source),
            source_tree,
            "{name}: checked-in fixture tree was never mutated"
        );
    }
}
