//! `artifact-mcp backup` and `artifact-mcp verify-backup`, ported from `test/backup.test.js`.
//!
//! Mutations that the old tests made while a fake `sqlite3` or `cp` paused the shell script are
//! made here from a [`Checkpoint`] observer, so the interleaving is deterministic. Exit codes,
//! messages, concurrency, and signal cleanup are checked against the real binary.

use std::{
    ffi::OsStr,
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant, SystemTime},
};

use artifact_mcp::ops::backup::{
    self, BackupError, BackupOptions, BackupReport, Cancel, Checkpoint, VerifyOptions,
};
use rusqlite::{Connection, OpenFlags, params};
use sha2::{Digest as _, Sha256};

const SCHEMA: &str = "CREATE TABLE artifacts (id TEXT PRIMARY KEY, is_bundle INTEGER NOT NULL, body_sha256 TEXT NOT NULL); \
    CREATE TABLE artifact_revisions (artifact_id TEXT NOT NULL, revision INTEGER NOT NULL, is_bundle INTEGER NOT NULL, body_sha256 TEXT NOT NULL); \
    CREATE TABLE artifact_durability_intents (id TEXT PRIMARY KEY);";

fn hash(content: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(content.as_ref()))
}

struct Fixture {
    root: PathBuf,
    data: PathBuf,
    backups: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("artifact-backup-{}", nanoid::nanoid!(12)));
        let data = root.join("data");
        fs::create_dir_all(data.join("artifacts")).unwrap();
        Connection::open(data.join("artifacts.db"))
            .unwrap()
            .execute_batch(SCHEMA)
            .unwrap();
        let backups = root.join("backups");
        Self {
            root,
            data,
            backups,
        }
    }

    fn db(&self) -> Connection {
        let connection = Connection::open(self.data.join("artifacts.db")).unwrap();
        connection.busy_timeout(Duration::from_secs(5)).unwrap();
        connection
    }

    fn insert_artifact(&self, id: &str, bundle: bool, digest: &str) {
        self.db()
            .execute(
                "INSERT INTO artifacts VALUES (?1, ?2, ?3)",
                params![id, i64::from(bundle), digest],
            )
            .unwrap();
    }

    fn insert_revision(&self, id: &str, revision: i64, bundle: bool, digest: &str) {
        self.db()
            .execute(
                "INSERT INTO artifact_revisions VALUES (?1, ?2, ?3, ?4)",
                params![id, revision, i64::from(bundle), digest],
            )
            .unwrap();
    }

    fn write(&self, relative: &str, content: impl AsRef<[u8]>) {
        write_file(&self.data.join(relative), content);
    }

    fn options(&self, keep: usize) -> BackupOptions {
        BackupOptions {
            data_dir: self.data.clone(),
            dest_root: self.backups.clone(),
            keep,
        }
    }

    /// One in-process run with a checkpoint observer.
    fn run(
        &self,
        mut observer: impl FnMut(Checkpoint<'_>) -> Result<(), BackupError>,
    ) -> Result<BackupReport, BackupError> {
        backup::run_backup(&self.options(14), &Cancel::default(), &mut observer)
    }

    fn run_plain(&self) -> Result<BackupReport, BackupError> {
        self.run(|_| Ok(()))
    }

    /// Every entry in the destination root, hidden ones included.
    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.backups)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn cli_backup(&self, keep: &str) -> Output {
        cli([
            OsStr::new("backup"),
            self.data.as_os_str(),
            self.backups.as_os_str(),
            OsStr::new(keep),
        ])
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn write_file(path: &Path, content: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_artifact-mcp"));
    command
        .env_remove("ARTIFACT_MCP_DATA_DIR")
        .env_remove("ARTIFACT_MCP_BACKUP_DIR")
        .env_remove("ARTIFACT_MCP_BACKUP_TEST_PAUSE")
        .env_remove("ARTIFACT_MCP_BACKUP_TEST_PHASE");
    command
}

fn cli<I, S>(args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    command().args(args).output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn coherence_message(result: Result<BackupReport, BackupError>) -> String {
    match result {
        Err(BackupError::Coherence(message)) => message,
        other => panic!("expected a coherence failure, got {other:?}"),
    }
}

fn read_only(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

#[test]
fn active_publish_before_the_snapshot_cannot_yield_a_missing_body() {
    let fixture = Fixture::new();
    fixture.insert_artifact("before", false, &hash("before"));
    fixture.insert_revision("before", 1, false, &hash("previous"));
    fixture.write("artifacts/before.html", "before");
    fixture.write("artifacts/.history/before/1.html", "previous");

    let report = fixture
        .run(|point| {
            if let Checkpoint::BeforeSnapshot = point {
                fixture.insert_artifact("during", false, &hash("during"));
                fixture.write("artifacts/during.html", "during");
            }
            Ok(())
        })
        .unwrap();

    let snapshot = read_only(&report.path.join("artifacts.db"));
    let ids: Vec<String> = snapshot
        .prepare("SELECT id FROM artifacts ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(ids, ["before", "during"]);
    assert_eq!(
        fs::read_to_string(report.path.join("artifacts/during.html")).unwrap(),
        "during"
    );
    assert_eq!(
        fs::read_to_string(report.path.join("artifacts/.history/before/1.html")).unwrap(),
        "previous"
    );
    assert_eq!(
        fixture.entries(),
        [report.path.file_name().unwrap().to_string_lossy()]
    );
}

#[test]
fn canonical_bundle_and_retained_bundle_history_are_verified_and_previews_stay_optional() {
    let fixture = Fixture::new();
    // JSON.stringify of the sorted manifest, built by hand so the test does not reuse the code
    // under test.
    let manifest = format!(
        r#"[["assets/site.css","{}"],["index.html","{}"],["z.html","{}"]]"#,
        hash("css"),
        hash("index"),
        hash("z")
    );
    let retained = format!(r#"[["index.html","{}"]]"#, hash("old"));
    fixture.insert_artifact("bundle1", true, &hash(manifest));
    fixture.insert_revision("bundle1", 1, true, &hash(retained));
    fixture.write("artifacts/bundle1/z.html", "z");
    fixture.write("artifacts/bundle1/index.html", "index");
    fixture.write("artifacts/bundle1/assets/site.css", "css");
    fixture.write("artifacts/.history/bundle1/1/index.html", "old");
    // Transient lifecycle evidence is kept, as `cp -a` kept it.
    fixture.write("artifacts/.bundle1.trash-abc123/index.html", "trash");
    fixture.write("previews/bundle1.png", "preview-cache");

    // Modes and times are preserved.
    let css = fixture.data.join("artifacts/bundle1/assets/site.css");
    let past = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    fs::File::options()
        .write(true)
        .open(&css)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(past).set_accessed(past))
        .unwrap();
    fs::set_permissions(&css, fs::Permissions::from_mode(0o640)).unwrap();

    let report = fixture.run_plain().unwrap();
    let copied = report.path.join("artifacts/bundle1/assets/site.css");
    assert_eq!(fs::read_to_string(&copied).unwrap(), "css");
    let copied_metadata = fs::metadata(&copied).unwrap();
    assert_eq!(copied_metadata.modified().unwrap(), past);
    assert_eq!(copied_metadata.permissions().mode() & 0o7777, 0o640);
    assert_eq!(
        fs::read_to_string(
            report
                .path
                .join("artifacts/.bundle1.trash-abc123/index.html")
        )
        .unwrap(),
        "trash"
    );
    assert_eq!(
        hash(fs::read(report.path.join("previews/bundle1.png")).unwrap()),
        hash("preview-cache")
    );
    let preview_manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(report.path.join("preview-sha256.json")).unwrap())
            .unwrap();
    assert_eq!(
        preview_manifest,
        serde_json::json!({"version": 1, "files": {"bundle1.png": hash("preview-cache")}})
    );
    let summary = backup::verify_backup(
        &report.path,
        VerifyOptions {
            database_required: true,
            preview_manifest_required: true,
        },
    )
    .unwrap();
    assert_eq!(summary.artifacts, Some(1));
    assert_eq!(summary.current_bodies, 1);
    assert_eq!(summary.retained_bodies, 1);
    assert_eq!(summary.previews, 1);
}

#[test]
fn corruption_of_a_copied_preview_fails_closed_while_preserving_the_source_cache() {
    let fixture = Fixture::new();
    fixture.write("previews/bundle1.png", "preview-cache");
    let message = coherence_message(fixture.run(|point| {
        if let Checkpoint::AfterPreviewCopy { stage } = point {
            fs::write(stage.join("previews/bundle1.png"), "corrupted-preview").unwrap();
        }
        Ok(())
    }));
    assert_eq!(message, "optional preview digest mismatch: bundle1.png");
    assert!(fixture.entries().is_empty(), "{:?}", fixture.entries());
    assert_eq!(
        hash(fs::read(fixture.data.join("previews/bundle1.png")).unwrap()),
        hash("preview-cache")
    );
}

#[test]
fn optional_previews_may_disappear_before_their_copy_without_blocking_backup() {
    let fixture = Fixture::new();
    fixture.write("previews/bundle1.png", "preview-cache");
    let report = fixture
        .run(|point| {
            if let Checkpoint::BeforePreviewCopy { .. } = point {
                fs::remove_dir_all(fixture.data.join("previews")).unwrap();
            }
            Ok(())
        })
        .unwrap();
    assert!(!report.path.join("previews").exists());
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(report.path.join("preview-sha256.json")).unwrap())
            .unwrap();
    assert_eq!(manifest, serde_json::json!({"version": 1, "files": {}}));
}

#[test]
fn digest_or_required_body_failure_leaves_no_completed_backup_and_fixing_it_permits_retry() {
    let fixture = Fixture::new();
    fixture.insert_artifact("single1", false, &hash("stable"));
    fixture.insert_revision("single1", 1, false, &hash("history"));
    fixture.write("artifacts/single1.html", "stable");
    let message = coherence_message(fixture.run(|point| {
        if let Checkpoint::AfterSnapshot { .. } = point {
            fixture.write("artifacts/single1.html", "corrupt");
        }
        Ok(())
    }));
    assert_eq!(message, "digest mismatch: artifacts/single1.html");
    assert!(fixture.entries().is_empty());

    fixture.write("artifacts/single1.html", "stable");
    fixture.write("artifacts/.history/single1/1.html", "history");
    fixture.run_plain().unwrap();
    assert_eq!(fixture.entries().len(), 1);
}

#[test]
fn missing_current_or_retained_bodies_and_pending_intents_cannot_publish_a_recovery_point() {
    let fixture = Fixture::new();
    fixture.insert_artifact("required", false, &hash("current"));
    fixture.insert_revision("required", 1, false, &hash("previous"));

    let output = fixture.cli_backup("14");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output).trim(),
        "backup: coherence check failed: required body missing: artifacts/required.html"
    );
    assert!(fixture.entries().is_empty());

    fixture.write("artifacts/required.html", "current");
    let output = fixture.cli_backup("14");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output).trim(),
        "backup: coherence check failed: required body missing: artifacts/.history/required/1.html"
    );
    assert!(fixture.entries().is_empty());

    fixture.write("artifacts/.history/required/1.html", "previous");
    fixture
        .db()
        .execute(
            "INSERT INTO artifact_durability_intents VALUES ('pending-publish')",
            [],
        )
        .unwrap();
    let output = fixture.cli_backup("14");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output).trim(),
        "backup: coherence check failed: 1 pending durability intent(s)"
    );
    assert!(fixture.entries().is_empty());
    let pending: i64 = read_only(&fixture.data.join("artifacts.db"))
        .query_row(
            "SELECT count(*) FROM artifact_durability_intents",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending, 1, "the live intent is left for startup recovery");
}

#[test]
fn backup_paths_containing_sql_and_uri_punctuation_remain_valid() {
    let mut fixture = Fixture::new();
    let data = fixture.root.join("data ' %41 # ?");
    fs::rename(&fixture.data, &data).unwrap();
    fixture.data = data;
    fixture.backups = fixture.root.join("backups ' % # ?");
    fixture.insert_artifact("single1", false, &hash("body"));
    fixture.write("artifacts/single1.html", "body");

    let output = fixture.cli_backup("14");
    assert!(output.status.success(), "{}", stderr(&output));
    let entries = fixture.entries();
    assert_eq!(entries.len(), 1);
    let verified = cli([
        OsStr::new("verify-backup"),
        OsStr::new("--database-required"),
        OsStr::new("--preview-manifest-required"),
        fixture.backups.join(&entries[0]).as_os_str(),
    ]);
    assert!(verified.status.success(), "{}", stderr(&verified));
    assert!(stdout(&verified).contains("ok (artifacts=1, current bodies=1"));
}

#[test]
fn required_bodies_cannot_depend_on_symlinks_outside_the_completed_backup() {
    let fixture = Fixture::new();
    fixture.insert_artifact("linked", false, &hash("external"));
    let external = fixture.root.join("external");
    write_file(&external.join("linked.html"), "external");
    fs::remove_dir_all(fixture.data.join("artifacts")).unwrap();
    std::os::unix::fs::symlink(&external, fixture.data.join("artifacts")).unwrap();

    let output = fixture.cli_backup("14");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output).trim(),
        "backup: coherence check failed: required body depends on a symlink: artifacts/linked.html"
    );
    assert!(fixture.entries().is_empty());
}

#[test]
fn concurrent_backups_have_unique_names_and_retention_keeps_the_latest_completed_snapshot() {
    let fixture = Fixture::new();
    let children: Vec<_> = (0..3)
        .map(|_| {
            command()
                .args([
                    OsStr::new("backup"),
                    fixture.data.as_os_str(),
                    fixture.backups.as_os_str(),
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
    }
    let older = fixture.entries();
    assert_eq!(older.len(), 3);
    assert!(older.iter().all(|name| name.starts_with("backup-")));

    let output = fixture.cli_backup("1");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).matches("backup: pruned backup-").count(), 3);
    let latest = fixture.entries();
    assert_eq!(latest.len(), 1);
    assert!(!older.contains(&latest[0]));
}

#[test]
fn invalid_retention_and_failed_publication_cannot_leave_incomplete_backups() {
    let fixture = Fixture::new();
    for keep in ["0", "00", "-1", "invalid"] {
        let output = fixture.cli_backup(keep);
        assert_eq!(output.status.code(), Some(1), "KEEP={keep}");
        assert_eq!(
            stderr(&output).trim(),
            "backup: KEEP must be a positive integer"
        );
    }
    assert!(!fixture.backups.exists());

    // Occupy the final name with a non-empty directory so that the atomic rename fails.
    let mut decoy = String::new();
    let result = fixture.run(|point| {
        if let Checkpoint::BeforePublish { final_path, .. } = point {
            write_file(&final_path.join("occupied"), "x");
            decoy = final_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
        }
        Ok(())
    });
    match result {
        Err(BackupError::Io { action, .. }) => assert!(action.starts_with("cannot publish")),
        other => panic!("expected a publication failure, got {other:?}"),
    }
    assert_eq!(fixture.entries(), [decoy]);
}

#[test]
fn the_verifier_fails_closed_on_omitted_paths_and_required_missing_databases() {
    let fixture = Fixture::new();
    let omitted = cli(["verify-backup"]);
    assert_eq!(omitted.status.code(), Some(2));
    assert!(stderr(&omitted).starts_with("usage: artifact-mcp verify-backup"));

    let missing = cli([
        OsStr::new("verify-backup"),
        OsStr::new("--database-required"),
        fixture.root.as_os_str(),
    ]);
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(
        stderr(&missing).trim(),
        "backup: coherence check failed: database snapshot is missing"
    );

    let absent = cli([
        OsStr::new("verify-backup"),
        fixture.root.join("absent").as_os_str(),
    ]);
    assert_eq!(absent.status.code(), Some(1));
    assert_eq!(
        stderr(&absent).trim(),
        "backup: coherence check failed: backup root must be exactly one existing directory"
    );

    let link = fixture.root.join("link");
    std::os::unix::fs::symlink(&fixture.data, &link).unwrap();
    let linked = cli([OsStr::new("verify-backup"), link.as_os_str()]);
    assert_eq!(linked.status.code(), Some(1));
}

#[test]
fn deleting_a_current_body_after_the_database_cut_fails_closed_and_cleans_staging() {
    let fixture = Fixture::new();
    fixture.insert_artifact("gone", false, &hash("body"));
    fixture.write("artifacts/gone.html", "body");
    let message = coherence_message(fixture.run(|point| {
        if let Checkpoint::AfterSnapshot { .. } = point {
            fs::remove_file(fixture.data.join("artifacts/gone.html")).unwrap();
        }
        Ok(())
    }));
    assert_eq!(message, "required body missing: artifacts/gone.html");
    assert!(fixture.entries().is_empty());
    assert!(!fixture.data.join("artifacts/gone.html").exists());
}

#[test]
fn pruning_retained_history_after_the_database_cut_fails_closed_and_cleans_staging() {
    let fixture = Fixture::new();
    fixture.insert_artifact("history", false, &hash("current"));
    fixture.insert_revision("history", 1, false, &hash("old"));
    fixture.write("artifacts/history.html", "current");
    fixture.write("artifacts/.history/history/1.html", "old");
    let retained = fixture.data.join("artifacts/.history/history/1.html");
    let message = coherence_message(fixture.run(|point| {
        if let Checkpoint::AfterSnapshot { .. } = point {
            fs::remove_file(&retained).unwrap();
        }
        Ok(())
    }));
    assert_eq!(
        message,
        "required body missing: artifacts/.history/history/1.html"
    );
    assert!(fixture.entries().is_empty());
    assert!(!retained.exists());
}

#[test]
fn cancelling_after_the_database_cut_removes_incomplete_staging() {
    let fixture = Fixture::new();
    fixture.insert_artifact("survive", false, &hash("body"));
    fixture.write("artifacts/survive.html", "body");
    let cancel = Cancel::default();
    let mut saw_staging = false;
    let result = backup::run_backup(&fixture.options(14), &cancel, &mut |point: Checkpoint<
        '_,
    >| {
        if let Checkpoint::AfterSnapshot { stage } = point {
            saw_staging = stage.join("artifacts.db").is_file();
            cancel.cancel();
        }
        Ok(())
    });
    assert!(
        matches!(result, Err(BackupError::Interrupted)),
        "{result:?}"
    );
    assert!(saw_staging);
    assert!(fixture.entries().is_empty());
    assert_eq!(
        fs::read_to_string(fixture.data.join("artifacts/survive.html")).unwrap(),
        "body"
    );
}

/// The binary's signal path. The pause hook exists only in debug builds.
#[cfg(debug_assertions)]
#[test]
fn interrupting_the_binary_after_the_database_cut_removes_incomplete_staging() {
    let fixture = Fixture::new();
    fixture.insert_artifact("survive", false, &hash("body"));
    fixture.write("artifacts/survive.html", "body");
    let pause = fixture.root.join("pause");
    let mut child = command()
        .args([
            OsStr::new("backup"),
            fixture.data.as_os_str(),
            fixture.backups.as_os_str(),
            OsStr::new("14"),
        ])
        .env("ARTIFACT_MCP_BACKUP_TEST_PAUSE", &pause)
        .env("ARTIFACT_MCP_BACKUP_TEST_PHASE", "after-snapshot")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pause.exists() {
        assert!(
            Instant::now() < deadline,
            "backup did not reach the database cut"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let staging = fixture.entries();
    assert_eq!(staging.len(), 1);
    assert!(staging[0].starts_with(".incomplete-"));
    assert!(
        fixture
            .backups
            .join(&staging[0])
            .join("artifacts.db")
            .is_file()
    );

    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let status = child.wait().unwrap();
    let mut message = String::new();
    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut message).unwrap();
    assert_eq!(status.code(), Some(143), "{message}");
    assert!(message.contains("stopped by signal 15; no incomplete staging left"));
    assert!(fixture.entries().is_empty());
    assert_eq!(
        fs::read_to_string(fixture.data.join("artifacts/survive.html")).unwrap(),
        "body"
    );
}

#[test]
fn a_data_directory_without_a_database_is_backed_up_without_a_snapshot() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.data.join("artifacts.db")).unwrap();
    fixture.write("artifacts/loose.html", "loose");
    let output = fixture.cli_backup("14");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("artifacts=0 (no database), quick_check ok)"),
        "{}",
        stdout(&output)
    );
    let entries = fixture.entries();
    let backup_dir = fixture.backups.join(&entries[0]);
    assert!(!backup_dir.join("artifacts.db").exists());
    assert_eq!(
        fs::read_to_string(backup_dir.join("artifacts/loose.html")).unwrap(),
        "loose"
    );
    assert!(
        cli([OsStr::new("verify-backup"), backup_dir.as_os_str()])
            .status
            .success()
    );
    let required = cli([
        OsStr::new("verify-backup"),
        OsStr::new("--database-required"),
        backup_dir.as_os_str(),
    ]);
    assert_eq!(required.status.code(), Some(1));
}

#[test]
fn backup_rejects_a_missing_data_directory_and_extra_arguments() {
    let fixture = Fixture::new();
    let missing = cli([
        OsStr::new("backup"),
        fixture.root.join("absent").as_os_str(),
        fixture.backups.as_os_str(),
    ]);
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(
        stderr(&missing).trim(),
        format!(
            "backup: DATA_DIR '{}' not found",
            fixture.root.join("absent").display()
        )
    );
    let extra = cli(["backup", "a", "b", "1", "extra"]);
    assert_eq!(extra.status.code(), Some(2));
}

#[test]
fn verify_backup_detects_tampering_at_rest_and_passes_once_reverted() {
    let fixture = Fixture::new();
    let manifest = format!(r#"[["index.html","{}"]]"#, hash("index"));
    fixture.insert_artifact("bundle1", true, &hash(manifest));
    fixture.insert_artifact("single1", false, &hash("current"));
    fixture.insert_revision("single1", 1, false, &hash("previous"));
    fixture.write("artifacts/bundle1/index.html", "index");
    fixture.write("artifacts/single1.html", "current");
    fixture.write("artifacts/.history/single1/1.html", "previous");
    let report = fixture.run_plain().unwrap();
    let verify = || {
        cli([
            OsStr::new("verify-backup"),
            OsStr::new("--database-required"),
            OsStr::new("--preview-manifest-required"),
            report.path.as_os_str(),
        ])
    };
    assert!(verify().status.success());

    // (a) A copied body is deleted.
    let body = report.path.join("artifacts/single1.html");
    fs::remove_file(&body).unwrap();
    let output = verify();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("required body missing: artifacts/single1.html"));
    fs::write(&body, "current").unwrap();
    assert!(verify().status.success());

    // (b) One byte of a copied bundle file flips.
    let file = report.path.join("artifacts/bundle1/index.html");
    fs::write(&file, "indey").unwrap();
    let output = verify();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("digest mismatch: artifacts/bundle1"));
    fs::write(&file, "index").unwrap();
    assert!(verify().status.success());

    // (c) The snapshot carries a pending durability intent.
    let snapshot = Connection::open(report.path.join("artifacts.db")).unwrap();
    snapshot
        .execute(
            "INSERT INTO artifact_durability_intents VALUES ('pending')",
            [],
        )
        .unwrap();
    let output = verify();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("1 pending durability intent(s)"));
    snapshot
        .execute("DELETE FROM artifact_durability_intents", [])
        .unwrap();
    drop(snapshot);
    assert!(verify().status.success());
}
