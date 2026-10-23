//! `cargo xtask release manifest` and `verify-manifest` (ported from
//! `test/release-manifest.test.js`).

mod release_support;

use release_support::{TempDir, release, release_ok, sha256_hex, stderr};
use serde_json::{Value, json};

const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CREATED: &str = "2026-07-29T18:00:00Z";
const IMAGE_DIGEST: &str =
    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const IMMUTABLE_IMAGE: &str = "ghcr.io/agentshelf-oss/artifact-mcp@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn fixture_preflight() -> String {
    format!(
        r#"{{"schemaVersion":1,"image":"{IMMUTABLE_IMAGE}","fixtures":[{{"status":"passed"}}]}}"#
    )
}

fn manifest_args() -> Vec<&'static str> {
    vec![
        "manifest",
        "--out",
        "release-manifest.json",
        "--tag",
        "v1.6.0",
        "--version",
        "1.6.0",
        "--commit",
        COMMIT,
        "--created",
        CREATED,
        "--binary",
        "artifact-mcp",
        "--image-ref",
        "ghcr.io/agentshelf-oss/artifact-mcp:v1.6.0",
        "--image-digest",
        IMAGE_DIGEST,
        "--schema-min",
        "0",
        "--schema-current",
        "23",
        "--schema-max",
        "23",
        "--source-sbom",
        "source.spdx.json",
        "--image-sbom",
        "image.spdx.json",
        "--migration-notes",
        "migration-notes.md",
        "--fixture-preflight",
        "historical-fixture-preflight.json",
        "--repository",
        "AgentShelf-OSS/artifact-mcp",
        "--binary-attestation",
        "https://github.com/AgentShelf-OSS/artifact-mcp/attestations/1",
        "--image-attestation",
        "https://github.com/AgentShelf-OSS/artifact-mcp/attestations/2",
    ]
}

fn release_directory(label: &str, binary: &str, sbom: &str) -> TempDir {
    let directory = TempDir::new(label);
    directory.write("artifact-mcp", binary);
    directory.write("source.spdx.json", sbom);
    directory.write("image.spdx.json", sbom.replace("source", "image"));
    directory.write("migration-notes.md", "No schema migration.\n");
    directory.write(
        "historical-fixture-preflight.json",
        format!("{}\n", fixture_preflight()),
    );
    directory
}

fn read_manifest(directory: &TempDir) -> Value {
    serde_json::from_str(&directory.read("release-manifest.json")).unwrap()
}

fn write_manifest(directory: &TempDir, manifest: &Value) {
    directory.write(
        "release-manifest.json",
        format!("{}\n", serde_json::to_string_pretty(manifest).unwrap()),
    );
}

/// `verify-manifest` must exit 1 with `expected` in its message.
#[track_caller]
fn assert_rejected(directory: &TempDir, expected: &str) {
    let output = release(
        &["verify-manifest", "release-manifest.json"],
        &directory.path,
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let message = stderr(&output);
    assert!(
        message.starts_with("release manifest verification failed: ") && message.contains(expected),
        "expected {expected:?}, got {message:?}"
    );
}

/// The exact bytes `scripts/release/generate-manifest.mjs` wrote for these inputs.
const NODE_MANIFEST: &str = r#"{
  "schemaVersion": 2,
  "release": {
    "tag": "v1.6.0",
    "version": "1.6.0",
    "commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "created": "2026-07-29T18:00:00Z"
  },
  "image": {
    "reference": "ghcr.io/agentshelf-oss/artifact-mcp:v1.6.0",
    "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    "immutableReference": "ghcr.io/agentshelf-oss/artifact-mcp@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
  },
  "binary": {
    "path": "artifact-mcp",
    "sha256": "BINARY_SHA256"
  },
  "schema": {
    "minCompatible": 0,
    "current": 23,
    "maxCompatible": 23
  },
  "sboms": [
    {
      "kind": "source",
      "path": "source.spdx.json",
      "sha256": "SOURCE_SHA256",
      "format": "SPDX-2.3"
    },
    {
      "kind": "image",
      "path": "image.spdx.json",
      "sha256": "IMAGE_SHA256",
      "format": "SPDX-2.3"
    }
  ],
  "attestations": [
    {
      "kind": "github-build-provenance",
      "repository": "AgentShelf-OSS/artifact-mcp",
      "subject": "artifact-mcp",
      "reference": "https://github.com/AgentShelf-OSS/artifact-mcp/attestations/1",
      "verification": "gh attestation verify artifact-mcp --repo AgentShelf-OSS/artifact-mcp"
    },
    {
      "kind": "oci-build-provenance",
      "repository": "AgentShelf-OSS/artifact-mcp",
      "subject": "ghcr.io/agentshelf-oss/artifact-mcp@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "reference": "https://github.com/AgentShelf-OSS/artifact-mcp/attestations/2",
      "verification": "gh attestation verify oci://ghcr.io/agentshelf-oss/artifact-mcp@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb --repo AgentShelf-OSS/artifact-mcp"
    }
  ],
  "validation": {
    "migrationNotes": {
      "path": "migration-notes.md",
      "sha256": "NOTES_SHA256"
    },
    "historicalFixturePreflight": "exact-oci-historical-fixtures/v1",
    "historicalFixturePreflightReport": {
      "path": "historical-fixture-preflight.json",
      "sha256": "REPORT_SHA256"
    }
  }
}
"#;

#[test]
fn release_manifest_is_deterministic_and_self_verifying() {
    let directory = release_directory(
        "release-manifest",
        "native binary bytes\n",
        "{\"spdxVersion\":\"SPDX-2.3\"}\n",
    );
    release_ok(&manifest_args(), &directory.path);
    let first = directory.read("release-manifest.json");
    release_ok(&manifest_args(), &directory.path);
    assert_eq!(directory.read("release-manifest.json"), first);

    let digest = |name: &str| sha256_hex(std::fs::read(directory.join(name)).unwrap());
    let expected = NODE_MANIFEST
        .replace("BINARY_SHA256", &digest("artifact-mcp"))
        .replace("SOURCE_SHA256", &digest("source.spdx.json"))
        .replace("IMAGE_SHA256", &digest("image.spdx.json"))
        .replace("NOTES_SHA256", &digest("migration-notes.md"))
        .replace(
            "REPORT_SHA256",
            &digest("historical-fixture-preflight.json"),
        );
    assert_eq!(first, expected);

    let verified = release_ok(
        &["verify-manifest", "release-manifest.json"],
        &directory.path,
    );
    assert_eq!(verified, format!("verified v1.6.0 ({IMAGE_DIGEST})\n"));
    let manifest = read_manifest(&directory);
    assert_eq!(manifest["image"]["immutableReference"], IMMUTABLE_IMAGE);
    assert_eq!(manifest["schemaVersion"], 2);
    assert_eq!(
        manifest["validation"]["historicalFixturePreflight"],
        "exact-oci-historical-fixtures/v1"
    );
    assert_eq!(
        manifest["validation"]["historicalFixturePreflightReport"]["path"],
        "historical-fixture-preflight.json"
    );
    assert!(manifest.get("recovery").is_none());
}

#[test]
fn release_manifest_verification_rejects_a_changed_artifact() {
    let directory = release_directory("release-tamper", "first\n", "source\n");
    release_ok(&manifest_args(), &directory.path);
    directory.write("artifact-mcp", "changed\n");
    assert_rejected(&directory, "digest mismatch for binary");
}

#[test]
fn release_manifest_verification_rejects_redirected_image_identity_and_changed_migration_notes() {
    let directory = release_directory("release-identity", "native binary\n", "source\n");
    release_ok(&manifest_args(), &directory.path);
    let mut redirected = read_manifest(&directory);
    redirected["image"]["immutableReference"] =
        json!(format!("ghcr.io/agentshelf-oss/other@{IMAGE_DIGEST}"));
    write_manifest(&directory, &redirected);
    assert_rejected(&directory, "immutable image reference");

    release_ok(&manifest_args(), &directory.path);
    let mut cross_repository = read_manifest(&directory);
    for (index, attestation) in cross_repository["attestations"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        attestation["repository"] = json!("evil/repository");
        attestation["reference"] = json!(format!(
            "https://github.com/evil/repository/attestations/{}",
            index + 1
        ));
        attestation["verification"] = json!(if index == 0 {
            "gh attestation verify artifact-mcp --repo evil/repository".to_owned()
        } else {
            format!(
                "gh attestation verify oci://ghcr.io/agentshelf-oss/artifact-mcp@{IMAGE_DIGEST} \
                 --repo evil/repository"
            )
        });
    }
    write_manifest(&directory, &cross_repository);
    assert_rejected(
        &directory,
        "attestation repository does not match image repository",
    );

    release_ok(&manifest_args(), &directory.path);
    directory.write("migration-notes.md", "Changed after manifest creation.\n");
    assert_rejected(&directory, "migration notes");
}

#[test]
fn release_manifest_verification_rejects_missing_or_inconsistent_validation_evidence() {
    let directory = release_directory("release-validation", "native binary\n", "source\n");

    release_ok(&manifest_args(), &directory.path);
    let mut missing_validation = read_manifest(&directory);
    missing_validation
        .as_object_mut()
        .unwrap()
        .remove("validation");
    write_manifest(&directory, &missing_validation);
    assert_rejected(&directory, "invalid migration notes digest record");

    release_ok(&manifest_args(), &directory.path);
    let mut missing_report = read_manifest(&directory);
    missing_report["validation"]
        .as_object_mut()
        .unwrap()
        .remove("historicalFixturePreflightReport");
    write_manifest(&directory, &missing_report);
    assert_rejected(
        &directory,
        "invalid historical fixture preflight report digest record",
    );

    for (report, expected) in [
        (
            json!({
                "schemaVersion": 1,
                "image": format!("ghcr.io/agentshelf-oss/other@sha256:{}", "c".repeat(64)),
                "fixtures": [{ "status": "passed" }],
            }),
            "historical fixture preflight image does not match immutable release image",
        ),
        (
            // A `--binary` (native) preflight report is never release evidence.
            json!({
                "schemaVersion": 1,
                "image": format!("local-binary:/usr/local/bin/artifact-mcp@sha256:{}", "d".repeat(64)),
                "fixtures": [{ "status": "passed" }],
            }),
            "historical fixture preflight image does not match immutable release image",
        ),
        (
            json!({ "schemaVersion": 1, "image": IMMUTABLE_IMAGE, "fixtures": [{ "status": "failed" }] }),
            "historical fixture preflight has missing or failed cases",
        ),
        (
            json!({ "schemaVersion": 1, "image": IMMUTABLE_IMAGE, "fixtures": [] }),
            "historical fixture preflight has missing or failed cases",
        ),
        (
            json!({ "schemaVersion": 1, "image": IMMUTABLE_IMAGE, "fixtures": [{ "status": "passed" }], "error": "Error: boom" }),
            "historical fixture preflight did not complete cleanly",
        ),
        (
            json!({ "schemaVersion": 2, "image": IMMUTABLE_IMAGE, "fixtures": [{ "status": "passed" }] }),
            "historical fixture preflight did not complete cleanly",
        ),
    ] {
        release_ok(&manifest_args(), &directory.path);
        let report_bytes = format!("{report}\n");
        directory.write("historical-fixture-preflight.json", &report_bytes);
        let mut inconsistent = read_manifest(&directory);
        inconsistent["validation"]["historicalFixturePreflightReport"]["sha256"] =
            json!(sha256_hex(&report_bytes));
        write_manifest(&directory, &inconsistent);
        assert_rejected(&directory, expected);
        directory.write(
            "historical-fixture-preflight.json",
            format!("{}\n", fixture_preflight()),
        );
    }

    // A falsy `error` (here an empty string) did not fail the Node verifier either.
    release_ok(&manifest_args(), &directory.path);
    let report_bytes = format!(
        "{}\n",
        json!({ "schemaVersion": 1, "image": IMMUTABLE_IMAGE, "fixtures": [{ "status": "passed" }], "error": "" })
    );
    directory.write("historical-fixture-preflight.json", &report_bytes);
    let mut falsy_error = read_manifest(&directory);
    falsy_error["validation"]["historicalFixturePreflightReport"]["sha256"] =
        json!(sha256_hex(&report_bytes));
    write_manifest(&directory, &falsy_error);
    release_ok(
        &["verify-manifest", "release-manifest.json"],
        &directory.path,
    );
}

#[test]
fn verification_is_stricter_than_the_node_verifier_where_it_coerced_types() {
    let directory = release_directory("release-strict", "native binary\n", "source\n");
    // The Node verifier accepted each of these: its regular expressions converted an array to
    // its string, and its attestation-reference expression left `.` unescaped.
    type Mutation = (&'static str, fn(&mut Value), &'static str);
    let mutations: [Mutation; 3] = [
        (
            "array commit",
            |manifest| manifest["release"]["commit"] = json!([COMMIT]),
            "invalid commit",
        ),
        (
            "array digest",
            |manifest| manifest["image"]["digest"] = json!([IMAGE_DIGEST]),
            "invalid image digest",
        ),
        (
            "array reference",
            |manifest| {
                manifest["attestations"][0]["reference"] =
                    json!(["https://github.com/AgentShelf-OSS/artifact-mcp/attestations/1"]);
            },
            "invalid attestation reference",
        ),
    ];
    for (label, mutate, expected) in mutations {
        release_ok(&manifest_args(), &directory.path);
        let mut manifest = read_manifest(&directory);
        mutate(&mut manifest);
        write_manifest(&directory, &manifest);
        assert_rejected(&directory, expected);
        eprintln!("{label}: rejected");
    }

    let mut args = manifest_args();
    for (index, value) in [
        (32, "AgentShelf-OSS/artifact.mcp"),
        (
            34,
            "https://github.com/AgentShelf-OSS/artifact.mcp/attestations/1",
        ),
        (
            36,
            "https://github.com/AgentShelf-OSS/artifact.mcp/attestations/2",
        ),
    ] {
        args[index] = value;
    }
    args[14] = "ghcr.io/agentshelf-oss/artifact.mcp:v1.6.0";
    let preflight = fixture_preflight().replace("artifact-mcp@", "artifact.mcp@");
    directory.write(
        "historical-fixture-preflight.json",
        format!("{preflight}\n"),
    );
    release_ok(&args, &directory.path);
    release_ok(
        &["verify-manifest", "release-manifest.json"],
        &directory.path,
    );
    let mut wildcard = read_manifest(&directory);
    wildcard["attestations"][1]["reference"] =
        json!("https://github.com/AgentShelf-OSS/artifactXmcp/attestations/2");
    write_manifest(&directory, &wildcard);
    assert_rejected(&directory, "invalid attestation reference");
}

#[test]
fn verify_manifest_requires_exactly_one_path_and_exits_1() {
    let directory = TempDir::new("release-usage");
    for args in [
        &["verify-manifest"][..],
        &["verify-manifest", "a.json", "b.json"][..],
    ] {
        let output = release(args, &directory.path);
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr(&output).contains("usage: cargo xtask release verify-manifest"));
    }
    let output = release(&["verify-manifest", "missing.json"], &directory.path);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("cannot read JSON"));
}

#[test]
fn manifest_inputs_are_validated_like_the_node_generator() {
    let directory = release_directory("release-inputs", "native binary\n", "source\n");
    for (index, value, expected) in [
        (4, "1.6.0", "tag has an invalid format"),
        (6, "1.6.1", "tag must be v followed by version"),
        (
            8,
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "commit has an invalid format",
        ),
        (10, "2026-07-29 18:00:00Z", "created has an invalid format"),
        (16, "sha256:abc", "image digest has an invalid format"),
        (18, "-1", "--schema-min must be a non-negative integer"),
        (20, "1.5", "--schema-current must be a non-negative integer"),
        (
            22,
            "9007199254740992",
            "--schema-max must be a non-negative integer",
        ),
        (18, "24", "schema range must satisfy min <= current <= max"),
        (32, "AgentShelf-OSS", "repository has an invalid format"),
        (
            34,
            "http://github.com/x",
            "binary attestation has an invalid format",
        ),
        (
            36,
            "https://github.com/",
            "image attestation has an invalid format",
        ),
    ] {
        let mut args = manifest_args();
        args[index] = value;
        let output = release(&args, &directory.path);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{expected}: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains(&format!("error: {expected}")),
            "{}",
            stderr(&output)
        );
    }
    let output = release(&["manifest", "--out"], &directory.path);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("error: invalid arguments"));

    // `Number(string)` forms the Node generator accepted; each writes the plain integer.
    for (min, current, max) in [
        ("0x0", " 0x17 ", "0b10111"),
        ("-0", "", "1e1"),
        ("0", "0o27", "23.0"),
    ] {
        let mut args = manifest_args();
        args[18] = min;
        args[20] = current;
        args[22] = max;
        release_ok(&args, &directory.path);
        let manifest = read_manifest(&directory);
        let schema = &manifest["schema"];
        assert_eq!(schema["minCompatible"], 0);
        assert!(schema["current"].is_u64() && schema["maxCompatible"].is_u64());
    }
    let mut args = manifest_args();
    args[12] = "absent-binary";
    let missing = release(&args, &directory.path);
    assert_eq!(
        missing.status.code(),
        Some(1),
        "a missing input file is a runtime failure"
    );
}
