//! `cargo xtask release verify-fixtures-in-image` (ported from
//! `test/release-fixture-image.test.js`, plus the native `--binary` mode).

mod release_support;

use std::path::Path;

use release_support::{TempDir, release, repo_root, stderr, stdout};
use serde_json::Value;

fn committed_corpus() -> std::path::PathBuf {
    repo_root().join("conformance/fixtures/historical")
}

fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// `boundary-v00` from the committed corpus plus five empty `release-v*` directories: enough for
/// the listing check, and the run stops at the first case.
fn minimal_corpus() -> TempDir {
    let corpus = TempDir::new("fixture-corpus");
    copy_tree(
        &committed_corpus().join("boundary-v00"),
        &corpus.join("boundary-v00"),
    );
    for index in 1..=5 {
        std::fs::create_dir(corpus.join(&format!("release-v{index}"))).unwrap();
    }
    corpus
}

/// A stand-in server executable that logs one line and exits 3 at once.
fn failing_server(directory: &TempDir) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.join("fake-artifact-mcp");
    std::fs::write(
        &path,
        "#!/bin/sh\necho 'fake server: refusing to start' >&2\nexit 3\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_str().unwrap().to_owned()
}

fn flip_first_byte(path: &Path) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes[0] ^= 0x01;
    std::fs::write(path, bytes).unwrap();
}

fn run_local(binary: &str, corpus: &Path, out: &Path) -> std::process::Output {
    release(
        &[
            "verify-fixtures-in-image",
            "--binary",
            binary,
            "--fixtures",
            corpus.to_str().unwrap(),
            out.to_str().unwrap(),
        ],
        &repo_root(),
    )
}

fn read_report(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn a_changed_fixture_body_or_database_fails_before_the_candidate_boots() {
    for (relative, expected) in [
        (
            "boundary-v00/artifacts/singleb00.html",
            "Error: 0: source body manifest changed",
        ),
        (
            "boundary-v00/artifacts.db",
            "Error: 0: source database digest changed",
        ),
    ] {
        let corpus = minimal_corpus();
        flip_first_byte(&corpus.join(relative));
        let out = corpus.join("report.json");
        // The binary is never started: the source check runs first.
        let result = run_local(&failing_server(&corpus), &corpus.path, &out);
        assert_eq!(result.status.code(), Some(1), "{}", stderr(&result));
        let report = read_report(&out);
        assert_eq!(report["schemaVersion"], 1);
        assert_eq!(report["fixtures"], serde_json::json!([]));
        assert_eq!(report["error"], expected);
        assert!(report.get("completedAt").is_none());
        let image = report["image"].as_str().unwrap();
        assert!(image.starts_with("local-binary:/") && image.contains("@sha256:"));
    }
}

#[test]
fn a_candidate_that_never_becomes_healthy_fails_with_diagnostics() {
    let corpus = minimal_corpus();
    let out = corpus.join("report.json");
    let result = run_local(&failing_server(&corpus), &corpus.path, &out);
    assert_eq!(result.status.code(), Some(1));
    let message = stderr(&result);
    assert!(
        message.contains("health never became ready")
            && message.contains("the process exited with exit status: 3")
            && message.contains("logs: fake server: refusing to start"),
        "{message}"
    );
    let report = read_report(&out);
    let error = report["error"].as_str().unwrap();
    assert!(error.starts_with("Error: artifact-mcp-fixture-"), "{error}");
    assert!(error.contains("-boundary-v00: health never became ready"));
}

#[test]
fn invalid_invocations_exit_1_without_a_report() {
    let directory = TempDir::new("fixture-usage");
    for args in [
        &["verify-fixtures-in-image"][..],
        &["verify-fixtures-in-image", "ghcr.io/o/r:v1", "out.json"][..],
        &["verify-fixtures-in-image", "ghcr.io/o/r@sha256:abc"][..],
        &[
            "verify-fixtures-in-image",
            "--fixtures",
            "/tmp",
            "ghcr.io/o/r@sha256:abc",
            "out.json",
        ][..],
        &[
            "verify-fixtures-in-image",
            "--binary",
            "/nonexistent/artifact-mcp",
            "out.json",
        ][..],
    ] {
        let result = release(args, &directory.path);
        assert_eq!(result.status.code(), Some(1), "{args:?}");
        assert!(!directory.join("out.json").exists());
    }
    let corpus = TempDir::new("fixture-gap");
    for name in ["boundary-v00", "boundary-v02"] {
        std::fs::create_dir(corpus.join(name)).unwrap();
    }
    for index in 1..=5 {
        std::fs::create_dir(corpus.join(&format!("release-v{index}"))).unwrap();
    }
    let out = directory.join("out.json");
    let result = run_local(&failing_server(&directory), &corpus.path, &out);
    assert_eq!(result.status.code(), Some(1));
    assert!(
        stderr(&result)
            .contains("historical fixture boundaries are not contiguous at boundary-v02")
    );
    assert!(!out.exists());
}

/// Boots every committed fixture with a native release binary:
/// `ARTIFACT_MCP_BINARY=target/release/artifact-mcp cargo test -p xtask --test release_fixtures -- --ignored`
#[test]
#[ignore = "needs ARTIFACT_MCP_BINARY, a built artifact-mcp release binary"]
fn every_committed_fixture_passes_against_a_native_binary() {
    let binary = std::env::var("ARTIFACT_MCP_BINARY")
        .expect("set ARTIFACT_MCP_BINARY to a built artifact-mcp binary");
    let out = TempDir::new("fixture-native");
    let report_path = out.join("historical-fixture-preflight.json");
    let result = release(
        &[
            "verify-fixtures-in-image",
            "--binary",
            &binary,
            report_path.to_str().unwrap(),
        ],
        &repo_root(),
    );
    assert!(result.status.success(), "{}", stderr(&result));
    let report = read_report(&report_path);
    let fixtures = report["fixtures"].as_array().unwrap();
    let corpus_size = std::fs::read_dir(committed_corpus())
        .unwrap()
        .filter(|entry| entry.as_ref().unwrap().file_type().unwrap().is_dir())
        .count();
    assert_eq!(fixtures.len(), corpus_size);
    assert_eq!(
        stdout(&result),
        format!("candidate fixture preflight passed: {corpus_size} cases\n")
    );
    assert!(fixtures.iter().all(|fixture| fixture["status"] == "passed"));
    assert!(report["completedAt"].as_str().unwrap().ends_with('Z'));
    assert!(report.get("error").is_none());
}
