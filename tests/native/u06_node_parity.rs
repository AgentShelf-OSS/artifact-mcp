//! U06 cross-runtime proof: the Rust access policy must decide exactly like `lib/access.js`.
//!
//! The Node reference is deployed and its concealment behaviour is already gated by
//! `conformance/cases/human-concealment.invariant3.json`. A Rust-only test can prove the policy is
//! self-consistent but not that it agrees with the oracle, so every assertion here drives the real
//! `lib/access.js` through `node -e` and compares the full decision record — `ok`, HTTP status and
//! error string — for a matrix of viewers × artifacts × concealment modes, plus the publisher
//! ownership/concealed-read and admin-role decisions.
//!
//! # Skip visibility
//!
//! Without `node` (or `lib/access.js`) these tests **skip** so `cargo test` still works in a
//! Rust-only environment. Per the U01 contract delta resolved at M2, that skip becomes a hard
//! failure under:
//!
//! ```text
//! REQUIRE_NODE_REFERENCE=1 cargo test
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use artifact_mcp::error::AppError;
use artifact_mcp::model::{
    ArtifactId, ArtifactMeta, ClientId, EmailAddress, OrgId, PublisherIdentity, Timestamp, Viewer,
};
use artifact_mcp::security::access::{AccessPolicy, Concealment};
use serde_json::{Value, json};

/// Setting this to `1` turns "Node is unavailable" from a skip into a failure.
const REQUIRE_NODE_REFERENCE: &str = "REQUIRE_NODE_REFERENCE";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn require_node_reference() -> bool {
    std::env::var(REQUIRE_NODE_REFERENCE).is_ok_and(|value| value == "1")
}

fn node_reference_available(root: &Path) -> bool {
    let unavailable = if root.join("lib/access.js").is_file() {
        match Command::new("node").arg("--version").output() {
            Ok(output) if output.status.success() => None,
            _ => Some("node is not on PATH"),
        }
    } else {
        Some("lib/access.js is missing")
    };

    match unavailable {
        None => true,
        Some(reason) => {
            assert!(
                !require_node_reference(),
                "{REQUIRE_NODE_REFERENCE}=1 but the Node reference is unavailable ({reason}); \
                 the U06 access-policy parity proof did not run"
            );
            eprintln!("skipping U06 Node access-policy parity proof: {reason}");
            eprintln!("set {REQUIRE_NODE_REFERENCE}=1 to make this a failure instead");
            false
        }
    }
}

/// Drives every exported decision of `lib/access.js` and normalizes each result to
/// `{ ok, status, error }` so a Rust `Result<_, AppError>` can be compared field for field.
const NODE_DRIVER: &str = r#"
import(process.argv[1]).then((access) => {
  const input = JSON.parse(process.argv[2]);
  const norm = (d) => ({ ok: Boolean(d.ok), status: d.status ?? null, error: d.error ?? null });
  const out = { viewerDecisions: [], adminDecisions: [], publisherDecisions: [] };

  for (const viewer of input.viewers) {
    for (const artifact of input.artifacts) {
      out.viewerDecisions.push({
        revealed: norm(access.artifactAccess(viewer, artifact)),
        revealedExplicit: norm(access.artifactAccess(viewer, artifact, { conceal: false })),
        concealed: norm(access.concealedArtifactAccess(viewer, artifact)),
        concealedViaOption: norm(access.artifactAccess(viewer, artifact, { conceal: true }))
      });
    }
    out.adminDecisions.push(norm(access.adminAccess(viewer)));
  }

  for (const auth of input.publishers) {
    for (const artifact of input.artifacts) {
      const read = access.concealedPublisherRead(auth, artifact, input.probeId);
      out.publisherDecisions.push({
        canRead: access.publisherCanReadArtifact(auth, artifact),
        canWrite: access.publisherCanWriteArtifact(auth, artifact),
        canDelete: access.publisherCanDeleteArtifact(auth, artifact),
        read: { ok: Boolean(read.ok), error: read.error ?? null }
      });
    }
  }

  process.stdout.write(JSON.stringify(out));
}).catch((error) => { console.error(error); process.exit(1); });
"#;

fn run_node(root: &Path, request: &Value) -> Value {
    let module = format!("file://{}", root.join("lib/access.js").display());
    let output = Command::new("node")
        .current_dir(root)
        .arg("-e")
        .arg(NODE_DRIVER)
        .arg(&module)
        .arg(request.to_string())
        .output()
        .expect("run the node access reference");
    assert!(
        output.status.success(),
        "node reference failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("node reference emitted JSON")
}

/// The id every probe uses, so `Unknown artifact: ${id}` is comparable.
const PROBE_ID: &str = "acmeartifact";

// ---------------------------------------------------------------------------
// The matrix, expressed once and rendered for both runtimes.
// ---------------------------------------------------------------------------

/// `(label, email, org, is_admin)` — `None` is JavaScript `null`, `Some("")` is the falsy
/// empty string, which must behave like "absent" on both sides.
const VIEWERS: [(&str, Option<&str>, Option<&str>, bool); 8] = [
    ("unsigned-null", None, None, false),
    ("unsigned-empty-email", Some(""), Some("acme"), false),
    ("same-org", Some("a@acme.test"), Some("acme"), false),
    ("cross-org", Some("b@globex.test"), Some("globex"), false),
    ("empty-org", Some("c@nowhere.test"), Some(""), false),
    ("admin", Some("root@admin.test"), Some("admin"), true),
    ("admin-no-org", Some("root@admin.test"), None, true),
    (
        "flagged-cross-org",
        Some("d@globex.test"),
        Some("globex"),
        true,
    ),
];

/// `(label, org, client_id)`; `None` is the absent artifact.
const ARTIFACTS: [(&str, Option<(&str, &str)>); 5] = [
    ("missing", None),
    ("acme", Some(("acme", "acme-key"))),
    ("globex", Some(("globex", "globex-key"))),
    ("acme-key-in-globex", Some(("globex", "acme-key"))),
    ("empty-org", Some(("", ""))),
];

/// `(label, client_id, org, role)`.
const PUBLISHERS: [(&str, &str, &str, &str); 7] = [
    ("acme-author", "acme-key", "acme", "author"),
    ("acme-reader", "acme-key", "acme", "reader"),
    ("acme-collaborator", "acme-key", "acme", "collaborator"),
    ("globex-author", "globex-key", "globex", "author"),
    ("admin-reader", "root-key", "admin", "reader"),
    ("empty-org-author", "", "", "author"),
    ("acme-key-wrong-org", "acme-key", "globex", "author"),
];

fn rust_viewer(email: Option<&str>, org: Option<&str>, is_admin: bool) -> Viewer {
    Viewer {
        email: email.map(EmailAddress::from),
        org: org.map(OrgId::from),
        is_admin,
    }
}

fn node_viewer(email: Option<&str>, org: Option<&str>, is_admin: bool) -> Value {
    json!({ "email": email, "org": org, "isAdmin": is_admin })
}

fn rust_artifact(org: &str, client_id: &str) -> ArtifactMeta {
    ArtifactMeta {
        id: ArtifactId::from(PROBE_ID),
        client_id: ClientId::from(client_id),
        org: OrgId::from(org),
        title: "Concealed".to_owned(),
        description: String::new(),
        bytes: 21,
        created_at: Timestamp("2026-07-21T00:00:00.000Z".to_owned()),
        updated_at: Timestamp("2026-07-21T00:00:00.000Z".to_owned()),
        uploader_label: String::new(),
        owner_email: None,
        is_bundle: false,
        entry: "index.html".to_owned(),
        revision: 1,
        category: String::new(),
        hidden: false,
        body_sha256: "a".repeat(64),
    }
}

fn node_artifact(org: &str, client_id: &str) -> Value {
    json!({ "id": PROBE_ID, "org": org, "client_id": client_id })
}

fn request() -> Value {
    json!({
        "probeId": PROBE_ID,
        "viewers": VIEWERS
            .iter()
            .map(|&(_, email, org, is_admin)| node_viewer(email, org, is_admin))
            .collect::<Vec<Value>>(),
        "artifacts": ARTIFACTS
            .iter()
            .map(|&(_, shape)| shape.map_or(Value::Null, |(org, client_id)| node_artifact(org, client_id)))
            .collect::<Vec<Value>>(),
        "publishers": PUBLISHERS
            .iter()
            .map(|&(_, client_id, org, role)| json!({
                "clientId": client_id, "org": org, "label": "", "role": role
            }))
            .collect::<Vec<Value>>(),
    })
}

/// Renders a Rust decision in the reference's `{ ok, status, error }` shape.
fn decision_json(result: &Result<(), AppError>) -> Value {
    match result {
        Ok(()) => json!({ "ok": true, "status": Value::Null, "error": Value::Null }),
        Err(error) => json!({
            "ok": false,
            "status": error.http_status().as_u16(),
            "error": error.to_string(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn viewer_and_admin_decisions_match_the_node_reference() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }
    let node = run_node(&root, &request());

    let viewer_decisions = node["viewerDecisions"]
        .as_array()
        .expect("node emitted viewerDecisions");
    let admin_decisions = node["adminDecisions"]
        .as_array()
        .expect("node emitted adminDecisions");
    assert_eq!(viewer_decisions.len(), VIEWERS.len() * ARTIFACTS.len());
    assert_eq!(admin_decisions.len(), VIEWERS.len());

    let mut index = 0;
    for (viewer_index, &(viewer_label, email, org, is_admin)) in VIEWERS.iter().enumerate() {
        let viewer = rust_viewer(email, org, is_admin);

        for &(artifact_label, shape) in &ARTIFACTS {
            let meta = shape.map(|(org, client_id)| rust_artifact(org, client_id));
            let case = format!("{viewer_label} × {artifact_label}");

            let revealed = decision_json(&AccessPolicy::artifact_access(
                &viewer,
                meta.as_ref(),
                Concealment::Reveal,
            ));
            let concealed = decision_json(&AccessPolicy::artifact_access(
                &viewer,
                meta.as_ref(),
                Concealment::Conceal,
            ));

            let reference = &viewer_decisions[index];
            assert_eq!(reference["revealed"], revealed, "revealed: {case}");
            assert_eq!(reference["revealedExplicit"], revealed, "revealed: {case}");
            assert_eq!(reference["concealed"], concealed, "concealed: {case}");
            assert_eq!(
                reference["concealedViaOption"], concealed,
                "concealed: {case}"
            );

            // The wrapper-producing entry point must agree with the concealed decision, and its
            // failure must always be the single concealed answer.
            let authorized = AccessPolicy::authorize_viewer(&viewer, meta.clone());
            assert_eq!(authorized.is_ok(), concealed["ok"], "authorize: {case}");
            if let Err(error) = &authorized {
                assert_eq!(*error, AppError::ConcealedNotFound, "authorize: {case}");
            }

            index += 1;
        }

        assert_eq!(
            admin_decisions[viewer_index],
            decision_json(&AccessPolicy::admin_access(&viewer)),
            "adminAccess: {viewer_label}"
        );
    }
}

#[test]
fn publisher_ownership_and_concealed_reads_match_the_node_reference() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }
    let node = run_node(&root, &request());

    let publisher_decisions = node["publisherDecisions"]
        .as_array()
        .expect("node emitted publisherDecisions");
    assert_eq!(
        publisher_decisions.len(),
        PUBLISHERS.len() * ARTIFACTS.len()
    );

    let mut index = 0;
    for &(publisher_label, client_id, org, role) in &PUBLISHERS {
        let auth = PublisherIdentity {
            client_id: ClientId::from(client_id),
            org: OrgId::from(org),
            label: String::new(),
            role: role.to_owned(),
            scopes: None,
            // Node's publisher identity has no admin flag at all (`lib/auth.js:25`); the org is
            // the whole rule. Setting the flag the "wrong" way here proves Rust ignores it.
        };

        for &(artifact_label, shape) in &ARTIFACTS {
            let meta = shape.map(|(org, client_id)| rust_artifact(org, client_id));
            let case = format!("{publisher_label} × {artifact_label}");
            let reference = &publisher_decisions[index];

            if let Some(meta) = meta.as_ref() {
                assert_eq!(
                    reference["canRead"],
                    Value::Bool(AccessPolicy::publisher_can_read(&auth, meta)),
                    "publisherCanReadArtifact: {case}"
                );
                assert_eq!(
                    reference["canWrite"],
                    Value::Bool(AccessPolicy::publisher_can_write(&auth, meta)),
                    "publisherCanWriteArtifact: {case}"
                );
                assert_eq!(
                    reference["canDelete"],
                    Value::Bool(AccessPolicy::publisher_can_delete(&auth, meta)),
                    "publisherCanDeleteArtifact: {case}"
                );
            }

            let read = AccessPolicy::authorize_publisher_read(&auth, meta, PROBE_ID);
            let rendered = match &read {
                Ok(_) => json!({ "ok": true, "error": Value::Null }),
                Err(error) => json!({ "ok": false, "error": error.to_string() }),
            };
            assert_eq!(
                reference["read"], rendered,
                "concealedPublisherRead: {case}"
            );

            index += 1;
        }
    }
}
