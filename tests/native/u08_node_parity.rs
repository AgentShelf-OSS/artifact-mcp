//! U08 cross-runtime proof: the lifecycle must agree with the real `lib/store.js`.
//!
//! A Rust-only test cannot prove that entry auto-selection, metadata normalization, revision
//! bookkeeping, or — most importantly — the crash-recovery classification match the reference, so
//! every assertion here drives the actual Node store through `node -e`. The driver imports
//! `lib/store.js` and `lib/db.js` with `DATA_DIR` pointed at a throwaway directory, which is the
//! same code path production runs.
//!
//! # Skip visibility
//!
//! Without `node` or `node_modules/better-sqlite3` these tests **skip**, so `cargo test` still
//! works in a Rust-only environment. Per the U01 contract (§"RESOLVED at M2"),
//! `REQUIRE_NODE_REFERENCE=1` converts every skip into a hard failure, which is how CI must run
//! this suite:
//!
//! ```text
//! REQUIRE_NODE_REFERENCE=1 cargo test
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use artifact_mcp::artifacts::digest::bundle_manifest_digest;
use artifact_mcp::model::{ArtifactContent, ArtifactMeta, ArtifactUpdate, OrgId, PublishArtifact};
use artifact_mcp::ports::ArtifactService as _;
use serde_json::Value;

use crate::u03_support::TempDataDir;
use crate::u08_support::{Fixture, bundle_content, html_update, publisher, sha256_hex};

/// Setting this to `1` turns "Node is unavailable" from a skip into a failure.
const REQUIRE_NODE_REFERENCE: &str = "REQUIRE_NODE_REFERENCE";

const OLD: &str = "<p>OLD</p>";
const NEW: &str = "<p>NEW body, different length</p>";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn require_node_reference() -> bool {
    matches!(std::env::var(REQUIRE_NODE_REFERENCE).as_deref(), Ok("1"))
}

/// Node reference availability. Returns `false` (skip) only when `REQUIRE_NODE_REFERENCE=1` is
/// unset; otherwise it fails, so a CI job cannot green-pass without ever running the proof.
fn node_reference_available(root: &Path) -> bool {
    let unavailable = if !root.join("lib/store.js").is_file() {
        Some("lib/store.js is missing")
    } else if !root.join("node_modules/better-sqlite3").is_dir() {
        Some("node_modules/better-sqlite3 is missing")
    } else {
        match Command::new("node").arg("--version").output() {
            Ok(output) if output.status.success() => None,
            _ => Some("node is not on PATH"),
        }
    };

    match unavailable {
        None => true,
        Some(reason) => {
            assert!(
                !require_node_reference(),
                "{REQUIRE_NODE_REFERENCE}=1 but the Node reference is unavailable ({reason}); \
                 the U08 lifecycle parity proof did not run"
            );
            eprintln!("skipping U08 Node lifecycle parity proof: {reason}");
            eprintln!("set {REQUIRE_NODE_REFERENCE}=1 to make this a failure instead");
            false
        }
    }
}

/// One `node -e` driver covering every operation this unit ports. `process.argv[1]` and `[2]` are
/// the module URLs and `[3]` the JSON request, matching how `u03_cross_runtime.rs` and
/// `u04_crypto.rs` drive the reference.
const NODE_DRIVER: &str = r#"
(async () => {
  const store = await import(process.argv[1]);
  const dbModule = await import(process.argv[2]);
  const fs = await import("node:fs");
  const path = await import("node:path");
  const crypto = await import("node:crypto");
  const db = dbModule.default;
  const dir = dbModule.ARTIFACT_DIR;
  const input = JSON.parse(process.argv[3]);
  const sha256 = (value) => crypto.createHash("sha256").update(value).digest("hex");
  const meta = (id) => db.prepare("SELECT * FROM artifacts WHERE id = ?").get(id);
  const out = {};

  if (input.op === "bundles") {
    out.results = input.cases.map((testCase) => {
      const published = store.publishBundle({
        clientId: "client-1", org: "acme", uploaderLabel: "Fixture publisher",
        files: testCase.files, entry: testCase.entry === null ? undefined : testCase.entry,
        title: "T", description: "D", category: "docs"
      });
      const row = meta(published.id);
      return {
        entry: published.entry, bytes: published.bytes, files: published.files,
        metaEntry: row.entry, digest: row.body_sha256, isBundle: row.is_bundle
      };
    });
  }

  if (input.op === "metadata") {
    out.results = input.cases.map((testCase) => {
      const published = store.publish({
        clientId: "client-1", org: testCase.org, uploaderLabel: testCase.label,
        html: testCase.html, title: testCase.title, description: testCase.description,
        category: testCase.category
      });
      const row = meta(published.id);
      return {
        title: row.title, description: row.description, category: row.category,
        uploaderLabel: row.uploader_label, org: row.org, bytes: row.bytes,
        digest: row.body_sha256, entry: row.entry, isBundle: row.is_bundle,
        revision: row.revision, hidden: row.hidden
      };
    });
  }

  if (input.op === "update") {
    const published = store.publish({
      clientId: "client-1", org: "acme", uploaderLabel: "Fixture publisher",
      html: input.old, title: "T", description: "D", category: "docs"
    });
    const id = published.id;
    const noop = store.update({
      id, clientId: "client-1", org: "acme", html: input.old,
      title: "T", description: "D", category: "docs"
    });
    out.noop = { ok: noop.ok, changed: noop.changed, revision: noop.revision };
    out.noopRevisionRows = db
      .prepare("SELECT COUNT(*) AS n FROM artifact_revisions WHERE artifact_id = ?").get(id).n;

    const changed = store.update({ id, clientId: "client-1", org: "acme", html: input.next });
    out.changed = { ok: changed.ok, changed: changed.changed, revision: changed.revision, bytes: changed.bytes };
    const row = meta(id);
    out.meta = { revision: row.revision, bytes: row.bytes, digest: row.body_sha256 };
    const revision = db
      .prepare("SELECT * FROM artifact_revisions WHERE artifact_id = ? AND revision = 1").get(id);
    out.revisionRow = {
      revision: revision.revision, bytes: revision.bytes, digest: revision.body_sha256,
      isBundle: revision.is_bundle, entry: revision.entry, title: revision.title
    };
    out.historyBody = fs.readFileSync(path.join(dir, ".history", id, "1.html"), "utf8");
    out.liveBody = fs.readFileSync(path.join(dir, `${id}.html`), "utf8");

    const stale = store.update({ id, clientId: "client-1", org: "acme", expectedRevision: 1, html: "stale" });
    out.stale = { ok: stale.ok, reason: stale.reason };
    out.staleTransients = fs.readdirSync(dir).filter((n) => n.includes(".staging-")).length;

    const restored = store.restore({ id, revision: 1, clientId: "client-1" });
    out.restored = {
      ok: restored.ok, revision: restored.revision, restoredFrom: restored.restoredFrom,
      bytes: restored.bytes
    };
    out.restoredBody = fs.readFileSync(path.join(dir, `${id}.html`), "utf8");
    out.restoredDigest = meta(id).body_sha256;
  }

  if (input.op === "audit") {
    const labels = {};
    const publishOne = (label, html) => {
      const published = store.publish({
        clientId: "client-1", org: "acme", uploaderLabel: "Fixture publisher",
        html, title: "T", description: "D", category: "docs"
      });
      labels[published.id] = label;
      return published.id;
    };
    const a1 = publishOne("a1", input.old);
    const a2 = publishOne("a2", input.old);
    const a3 = publishOne("a3", input.old);
    const a4 = publishOne("a4", input.old);
    const a5 = publishOne("a5", input.old);

    // a1: an interrupted swap whose final path is empty.
    fs.renameSync(path.join(dir, `${a1}.html`), path.join(dir, `.${a1}.staging-aaaaaaaaaaaa`));
    // a2: the commit-then-swap window — metadata already at the new digest.
    db.prepare("UPDATE artifacts SET body_sha256 = ? WHERE id = ?").run(sha256(input.next), a2);
    fs.writeFileSync(path.join(dir, `.${a2}.staging-bbbbbbbbbbbb`), input.next, "utf8");
    // a3: an interrupted delete whose row survived.
    fs.renameSync(path.join(dir, `${a3}.html`), path.join(dir, `.${a3}.trash-cccccccccccc`));
    // a4: a body with no record.
    db.prepare("DELETE FROM artifacts WHERE id = ?").run(a4);
    // a5: a record with no body.
    fs.rmSync(path.join(dir, `${a5}.html`));

    const report = store.auditStorage({ cleanTransient: true });
    const label = (value) => labels[String(value).replace(/\.html$/, "")] || String(value);
    out.report = {
      missingBodies: report.missingBodies.map(label).sort(),
      divergentBodies: report.divergentBodies.map(label).sort(),
      orphanBodies: report.orphanBodies.map(label).sort(),
      orphanHistory: report.orphanHistory.map(label).sort(),
      transientPaths: report.transientPaths.length,
      recoveredPaths: report.recoveredPaths.length
    };
    const bodyOf = (id) => {
      try { return fs.readFileSync(path.join(dir, `${id}.html`), "utf8"); } catch { return null; }
    };
    out.bodies = { a1: bodyOf(a1), a2: bodyOf(a2), a3: bodyOf(a3), a4: bodyOf(a4), a5: bodyOf(a5) };
    out.transientsLeft = fs.readdirSync(dir).filter((n) => n.startsWith(".") && n.includes("-")).length;
  }

  if (input.op === "crashSafety") {
    const prepareCommittedUpdate = (id) => {
      db.prepare(`
        INSERT OR REPLACE INTO artifact_revisions
          (artifact_id, org, revision, title, description, category, bytes, is_bundle, entry, body_sha256)
        SELECT id, org, revision, title, description, category, bytes, is_bundle, entry, body_sha256
        FROM artifacts WHERE id = ?
      `).run(id);
      db.prepare("UPDATE artifacts SET body_sha256 = ?, revision = 2 WHERE id = ?")
        .run(sha256(input.next), id);
    };
    const a2 = store.publish({
      clientId: "client-1", org: "acme", uploaderLabel: "Fixture publisher",
      html: input.old, title: "T", description: "D", category: "docs"
    }).id;
    const a3 = store.publish({
      clientId: "client-1", org: "acme", uploaderLabel: "Fixture publisher",
      html: input.old, title: "T", description: "D", category: "docs"
    }).id;
    prepareCommittedUpdate(a2);
    prepareCommittedUpdate(a3);
    const a2Staging = `.${a2}.staging-aaaaaaaaaaaa`;
    const a3Staging = `.${a3}.staging-bbbbbbbbbbbb`;
    fs.writeFileSync(path.join(dir, a2Staging), input.next, "utf8");
    fs.writeFileSync(path.join(dir, a3Staging), input.torn, "utf8");

    const report = store.auditStorage({ cleanTransient: true });
    out.report = {
      transientPaths: report.transientPaths.length,
      recoveredPaths: report.recoveredPaths.length,
      divergentBodies: report.divergentBodies.length,
      a2Recovered: report.recoveredPaths.includes(a2Staging),
      a3Transient: report.transientPaths.includes(a3Staging),
      a3Recovered: report.recoveredPaths.includes(a3Staging),
      a3Divergent: report.divergentBodies.includes(a3)
    };
    out.a2Live = fs.readFileSync(path.join(dir, `${a2}.html`), "utf8");
    out.a2History = fs.readFileSync(path.join(dir, ".history", a2, "1.html"), "utf8");
    out.a3Live = fs.readFileSync(path.join(dir, `${a3}.html`), "utf8");
    out.a3Staging = fs.readFileSync(path.join(dir, a3Staging), "utf8");
    const restored = store.restore({ id: a2, revision: 1, clientId: "client-1" });
    out.a2Restore = {
      ok: restored.ok, revision: restored.revision, restoredFrom: restored.restoredFrom,
      body: fs.readFileSync(path.join(dir, `${a2}.html`), "utf8")
    };
  }

  if (input.op === "backfill") {
    const single = store.publish({
      clientId: "client-1", org: "acme", uploaderLabel: "Fixture publisher",
      html: input.old, title: "T", description: "D", category: "docs"
    });
    const bundle = store.publishBundle({
      clientId: "client-1", org: "acme", uploaderLabel: "Fixture publisher",
      files: { "index.html": input.next }, title: "T", description: "D", category: "docs"
    });
    db.prepare("UPDATE artifacts SET body_sha256 = ''").run();
    const before = meta(single.id).updated_at;
    const first = store.backfillBodyDigests();
    const second = store.backfillBodyDigests();
    out.first = first;
    out.second = second;
    out.singleDigest = meta(single.id).body_sha256;
    out.bundleDigest = meta(bundle.id).body_sha256;
    out.revision = meta(single.id).revision;
    out.updatedAtUnchanged = meta(single.id).updated_at === before;
  }

  process.stdout.write(JSON.stringify(out));
})().catch((error) => { console.error(error); process.exit(1); });
"#;

/// Runs the driver against a fresh `DATA_DIR`. `request` is a pre-serialized JSON string because
/// bundle file order is load-bearing and `serde_json::Value` would sort the keys.
fn run_node(root: &Path, request: &str) -> Value {
    let data_dir = TempDataDir::new("u08-node");
    let store_url = format!("file://{}", root.join("lib/store.js").display());
    let db_url = format!("file://{}", root.join("lib/db.js").display());
    let output = Command::new("node")
        .current_dir(root)
        .arg("-e")
        .arg(NODE_DRIVER)
        .arg(&store_url)
        .arg(&db_url)
        .arg(request)
        .env("DATA_DIR", data_dir.path())
        .env_remove("WEBHOOK_ENC_KEY")
        .env_remove("ARTIFACT_API_KEYS")
        .env_remove("ORG_EMAIL_DOMAINS")
        .env_remove("MAX_HISTORY")
        .output()
        .expect("run the node store reference");
    assert!(
        output.status.success(),
        "node reference failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("node reference emitted JSON")
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("string serializes")
}

/// A JSON object literal whose key order is preserved — the whole point of contract delta 4.
fn files_object(files: &[(&str, &str)]) -> String {
    let entries: Vec<String> = files
        .iter()
        .map(|(name, body)| format!("{}:{}", json_string(name), json_string(body)))
        .collect();
    format!("{{{}}}", entries.join(","))
}

fn results(response: &Value) -> &Vec<Value> {
    response
        .get("results")
        .and_then(Value::as_array)
        .expect("the driver returned a results array")
}

fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    value
        .get(name)
        .unwrap_or_else(|| panic!("missing field {name} in {value}"))
}

fn text(value: &Value, name: &str) -> String {
    field(value, name)
        .as_str()
        .unwrap_or_else(|| panic!("field {name} is not a string in {value}"))
        .to_owned()
}

fn number(value: &Value, name: &str) -> u64 {
    field(value, name)
        .as_u64()
        .unwrap_or_else(|| panic!("field {name} is not a number in {value}"))
}

fn labels(value: &Value, name: &str) -> Vec<String> {
    field(value, name)
        .as_array()
        .unwrap_or_else(|| panic!("field {name} is not an array in {value}"))
        .iter()
        .map(|entry| entry.as_str().unwrap_or_default().to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// bundle entry selection and digests
// ---------------------------------------------------------------------------

/// An ordered bundle payload plus the entry the publisher asked for, if any.
type BundleCase = (
    &'static [(&'static str, &'static str)],
    Option<&'static str>,
);

/// The cases that separate publisher order from any sorted container (contract delta 4).
const BUNDLE_CASES: [BundleCase; 6] = [
    (&[("z.html", "Z"), ("a.html", "A")], None),
    (&[("a.html", "A"), ("z.html", "Z")], None),
    (
        &[("z.html", "Z"), ("index.html", "I"), ("a.html", "A")],
        None,
    ),
    (&[("z.html", "Z"), ("a.html", "A")], Some("a.html")),
    (&[("b.htm", "B"), ("c.html", "C")], None),
    (&[("nested/deep/page.html", "P"), ("style.css", "S")], None),
];

#[tokio::test]
async fn bundle_entry_selection_and_digests_match_the_node_oracle() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let cases: Vec<String> = BUNDLE_CASES
        .iter()
        .map(|(files, entry)| {
            format!(
                "{{\"files\":{},\"entry\":{}}}",
                files_object(files),
                entry.map_or_else(|| "null".to_owned(), json_string)
            )
        })
        .collect();
    let response = run_node(
        &root,
        &format!("{{\"op\":\"bundles\",\"cases\":[{}]}}", cases.join(",")),
    );
    let node = results(&response);
    assert_eq!(node.len(), BUNDLE_CASES.len());

    let fixture = Fixture::new("parity-bundles");
    for (index, (files, entry)) in BUNDLE_CASES.iter().enumerate() {
        let published = fixture.publish_bundle(files, *entry).await;
        let expected = &node[index];
        let case = format!("bundle case {index}: {files:?} entry={entry:?}");

        assert_eq!(published.meta.entry, text(expected, "entry"), "{case}");
        assert_eq!(published.meta.entry, text(expected, "metaEntry"), "{case}");
        assert_eq!(published.meta.bytes, number(expected, "bytes"), "{case}");
        assert_eq!(
            published.file_count.unwrap_or_default() as u64,
            number(expected, "files"),
            "{case}"
        );
        assert_eq!(
            published.meta.body_sha256,
            text(expected, "digest"),
            "{case}: the canonical bundle digest must be byte identical"
        );
        assert_eq!(number(expected, "isBundle"), 1, "{case}");
        assert!(published.meta.is_bundle, "{case}");
    }
}

// ---------------------------------------------------------------------------
// metadata normalization
// ---------------------------------------------------------------------------

struct MetadataCase {
    html: &'static str,
    title: Option<&'static str>,
    description: Option<&'static str>,
    category: Option<&'static str>,
    label: &'static str,
    org: &'static str,
}

fn metadata_cases() -> Vec<MetadataCase> {
    vec![
        MetadataCase {
            html: "<p>plain</p>",
            title: Some("Plain title"),
            description: Some("A description"),
            category: Some("docs"),
            label: "Fixture publisher",
            org: "acme",
        },
        MetadataCase {
            html: "<p>defaults</p>",
            title: None,
            description: None,
            category: None,
            label: "",
            org: "",
        },
        MetadataCase {
            html: "<p>blank</p>",
            title: Some(""),
            description: Some(""),
            category: Some("   spaced    out   "),
            label: "l",
            org: "acme",
        },
        MetadataCase {
            html: "<p>unicode</p>",
            title: Some("Ünïcødé — 🎉 title"),
            description: Some("déscriptïon 🎉"),
            category: Some("caté gory"),
            label: "Ünïcødé label",
            org: "acme",
        },
    ]
}

#[tokio::test]
async fn metadata_normalization_matches_the_node_oracle() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let cases = metadata_cases();
    let payload: Vec<Value> = cases
        .iter()
        .map(|case| {
            let mut object = serde_json::Map::new();
            object.insert("html".to_owned(), Value::String(case.html.to_owned()));
            object.insert("label".to_owned(), Value::String(case.label.to_owned()));
            object.insert("org".to_owned(), Value::String(case.org.to_owned()));
            for (key, value) in [
                ("title", case.title),
                ("description", case.description),
                ("category", case.category),
            ] {
                // An absent key is Node's `undefined`, which is a different input from "".
                if let Some(value) = value {
                    object.insert(key.to_owned(), Value::String(value.to_owned()));
                }
            }
            Value::Object(object)
        })
        .collect();
    let request = serde_json::json!({ "op": "metadata", "cases": payload }).to_string();
    let response = run_node(&root, &request);
    let node = results(&response);

    let fixture = Fixture::new("parity-metadata");
    for (index, case) in cases.iter().enumerate() {
        let request = PublishArtifact {
            publisher: artifact_mcp::model::PublisherIdentity {
                label: case.label.to_owned(),
                ..publisher()
            },
            target_org: OrgId(case.org.to_owned()),
            title: case.title.map(ToOwned::to_owned),
            description: case.description.map(ToOwned::to_owned),
            category: case.category.map(ToOwned::to_owned),
            content: ArtifactContent::SingleHtml(case.html.to_owned()),
        };
        let meta = fixture
            .try_publish_request(request)
            .await
            .expect("publish succeeds")
            .meta;
        let expected = &node[index];
        let context = format!("metadata case {index}");

        assert_eq!(meta.title, text(expected, "title"), "{context}: title");
        assert_eq!(
            meta.description,
            text(expected, "description"),
            "{context}: description"
        );
        assert_eq!(
            meta.category,
            text(expected, "category"),
            "{context}: category"
        );
        assert_eq!(
            meta.uploader_label,
            text(expected, "uploaderLabel"),
            "{context}: uploader label"
        );
        assert_eq!(meta.org.0, text(expected, "org"), "{context}: org");
        assert_eq!(meta.bytes, number(expected, "bytes"), "{context}: bytes");
        assert_eq!(
            meta.body_sha256,
            text(expected, "digest"),
            "{context}: digest"
        );
        assert_eq!(meta.entry, text(expected, "entry"), "{context}: entry");
        assert_eq!(
            meta.revision,
            number(expected, "revision"),
            "{context}: revision"
        );
        assert_eq!(number(expected, "hidden"), 0, "{context}");
        assert!(!meta.hidden, "{context}");
    }
}

#[tokio::test]
async fn long_metadata_is_truncated_exactly_like_node() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    // 250 / 600 / 100 characters, past every `slice()` boundary in `metadata()`.
    let title = "t".repeat(250);
    let description = "d".repeat(600);
    let category = "c".repeat(100);
    let label = "l".repeat(80);
    let request = serde_json::json!({
        "op": "metadata",
        "cases": [{
            "html": "<p>long</p>",
            "title": title,
            "description": description,
            "category": category,
            "label": label,
            "org": "acme"
        }]
    })
    .to_string();
    let response = run_node(&root, &request);
    let expected = &results(&response)[0];

    let fixture = Fixture::new("parity-truncation");
    let meta = fixture
        .try_publish_request(PublishArtifact {
            publisher: artifact_mcp::model::PublisherIdentity {
                label: label.clone(),
                ..publisher()
            },
            target_org: OrgId("acme".to_owned()),
            title: Some(title),
            description: Some(description),
            category: Some(category),
            content: ArtifactContent::SingleHtml("<p>long</p>".to_owned()),
        })
        .await
        .expect("publish succeeds")
        .meta;

    assert_eq!(meta.title.len(), 200);
    assert_eq!(meta.description.len(), 500);
    assert_eq!(meta.category.len(), 60);
    assert_eq!(meta.uploader_label.len(), 60);
    assert_eq!(meta.title, text(expected, "title"));
    assert_eq!(meta.description, text(expected, "description"));
    assert_eq!(meta.category, text(expected, "category"));
    assert_eq!(meta.uploader_label, text(expected, "uploaderLabel"));
}

// ---------------------------------------------------------------------------
// update, revision bookkeeping, restore
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_and_restore_bookkeeping_matches_the_node_oracle() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let request = serde_json::json!({ "op": "update", "old": OLD, "next": NEW }).to_string();
    let response = run_node(&root, &request);

    let fixture = Fixture::new("parity-update");
    let meta = fixture.publish_single(OLD).await;

    // 1. An exact no-op creates no revision and no history snapshot, in both runtimes.
    let noop = fixture
        .store
        .update_for(
            &meta,
            ArtifactUpdate {
                expected_revision: 1,
                title: Some("Fixture".to_owned()),
                description: Some("Fixture artifact".to_owned()),
                category: Some("docs".to_owned()),
                content: Some(ArtifactContent::SingleHtml(OLD.to_owned())),
                acting_client_id: None,
            },
        )
        .await
        .expect("no-op update");
    let node_noop = field(&response, "noop");
    assert_eq!(node_noop.get("changed"), Some(&Value::Bool(false)));
    assert!(!noop.changed);
    assert_eq!(noop.meta.revision, number(node_noop, "revision"));
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM artifact_revisions") as u64,
        number(&response, "noopRevisionRows")
    );

    // 2. A real update bumps the revision and records the OUTGOING one.
    let changed = fixture
        .store
        .update_for(&meta, html_update(1, NEW))
        .await
        .expect("body update");
    let node_meta = field(&response, "meta");
    assert!(changed.changed);
    assert_eq!(changed.meta.revision, number(node_meta, "revision"));
    assert_eq!(changed.meta.bytes, number(node_meta, "bytes"));
    assert_eq!(changed.meta.body_sha256, text(node_meta, "digest"));

    let node_revision = field(&response, "revisionRow");
    assert_eq!(
        fixture.scalar::<i64>("SELECT revision FROM artifact_revisions WHERE revision = 1") as u64,
        number(node_revision, "revision")
    );
    assert_eq!(
        fixture.scalar::<String>("SELECT body_sha256 FROM artifact_revisions WHERE revision = 1"),
        text(node_revision, "digest")
    );
    assert_eq!(
        fixture.history_body(&meta, 1).as_deref(),
        Some(text(&response, "historyBody").as_str())
    );
    assert_eq!(
        fixture.body_on_disk(&meta).as_deref(),
        Some(text(&response, "liveBody").as_str())
    );

    // 3. A stale expected revision conflicts and leaves no staged body behind.
    let stale = fixture
        .store
        .update_for(&meta, html_update(1, "stale"))
        .await
        .expect_err("stale revision");
    assert_eq!(
        stale,
        artifact_mcp::error::AppError::Conflict("conflict".to_owned())
    );
    assert_eq!(
        field(&response, "stale").get("reason"),
        Some(&Value::String("conflict".to_owned()))
    );
    assert_eq!(
        fixture.staging_entries().len() as u64,
        number(&response, "staleTransients")
    );

    // 4. Restore replays revision 1 as a NEW revision.
    let current = fixture.reload(&meta).expect("row");
    let restored = fixture
        .store
        .restore_for(&current, 1, None)
        .await
        .expect("restore succeeds");
    let node_restored = field(&response, "restored");
    assert_eq!(restored.meta.revision, number(node_restored, "revision"));
    assert_eq!(
        restored.restored_from,
        number(node_restored, "restoredFrom")
    );
    assert_eq!(restored.meta.bytes, number(node_restored, "bytes"));
    assert_eq!(
        fixture.body_on_disk(&meta).as_deref(),
        Some(text(&response, "restoredBody").as_str())
    );
    assert_eq!(restored.meta.body_sha256, text(&response, "restoredDigest"));
}

// ---------------------------------------------------------------------------
// crash recovery classification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn storage_reconciliation_classifies_crash_states_like_the_node_oracle() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let request = serde_json::json!({ "op": "audit", "old": OLD, "next": NEW }).to_string();
    let response = run_node(&root, &request);
    let node_report = field(&response, "report");

    // The identical five pre-states, built against the Rust store.
    let fixture = Fixture::new("parity-audit");
    let mut prepared: BTreeMap<&str, ArtifactMeta> = BTreeMap::new();
    for label in ["a1", "a2", "a3", "a4", "a5"] {
        prepared.insert(label, fixture.publish_single(OLD).await);
    }
    let path_of = |meta: &ArtifactMeta| fixture.artifact_dir.join(format!("{}.html", meta.id.0));

    let a1 = &prepared["a1"];
    std::fs::rename(
        path_of(a1),
        fixture
            .artifact_dir
            .join(format!(".{}.staging-aaaaaaaaaaaa", a1.id.0)),
    )
    .expect("park a1 in staging");

    let a2 = &prepared["a2"];
    fixture.execute(&format!(
        "UPDATE artifacts SET body_sha256 = '{}' WHERE id = '{}'",
        sha256_hex(NEW),
        a2.id.0
    ));
    std::fs::write(
        fixture
            .artifact_dir
            .join(format!(".{}.staging-bbbbbbbbbbbb", a2.id.0)),
        NEW,
    )
    .expect("stage a2");

    let a3 = &prepared["a3"];
    std::fs::rename(
        path_of(a3),
        fixture
            .artifact_dir
            .join(format!(".{}.trash-cccccccccccc", a3.id.0)),
    )
    .expect("park a3 in trash");

    let a4 = &prepared["a4"];
    fixture.execute(&format!("DELETE FROM artifacts WHERE id = '{}'", a4.id.0));

    let a5 = &prepared["a5"];
    std::fs::remove_file(path_of(a5)).expect("remove a5's body");

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");

    let to_labels = |values: &[String]| {
        let mut out: Vec<String> = values
            .iter()
            .map(|value| {
                let id = value.trim_end_matches(".html");
                prepared
                    .iter()
                    .find(|(_, meta)| meta.id.0 == id)
                    .map_or_else(|| value.clone(), |(label, _)| (*label).to_owned())
            })
            .collect();
        out.sort();
        out
    };

    assert_eq!(
        to_labels(&report.missing_bodies),
        labels(node_report, "missingBodies"),
        "missing bodies"
    );
    assert_eq!(
        to_labels(&report.divergent_bodies),
        labels(node_report, "divergentBodies"),
        "divergent bodies"
    );
    assert_eq!(
        to_labels(&report.orphan_bodies),
        labels(node_report, "orphanBodies"),
        "orphan bodies"
    );
    assert_eq!(
        to_labels(&report.orphan_history),
        labels(node_report, "orphanHistory"),
        "orphan history"
    );
    assert_eq!(
        report.transient_paths.len() as u64,
        number(node_report, "transientPaths"),
        "transient paths"
    );
    assert_eq!(
        report.recovered_paths.len() as u64,
        number(node_report, "recoveredPaths"),
        "recovered paths"
    );

    // And the recovered content itself agrees, artifact by artifact.
    let node_bodies = field(&response, "bodies");
    for label in ["a1", "a2", "a3", "a4", "a5"] {
        let expected = node_bodies
            .get(label)
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned);
        assert_eq!(
            fixture.body_on_disk(&prepared[label]),
            expected,
            "recovered body for {label}"
        );
    }
    assert_eq!(
        fixture.transient_entries().len() as u64,
        number(&response, "transientsLeft"),
        "leftover transient paths"
    );
}

#[tokio::test]
async fn crash_safe_revision_and_staging_recovery_matches_the_node_oracle() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let request = serde_json::json!({
        "op": "crashSafety",
        "old": OLD,
        "next": NEW,
        "torn": ""
    })
    .to_string();
    let response = run_node(&root, &request);
    let node_report = field(&response, "report");

    let fixture = Fixture::new("parity-crash-safety");
    let a2 = fixture.publish_single(OLD).await;
    let a3 = fixture.publish_single(OLD).await;
    for meta in [&a2, &a3] {
        fixture.execute(&format!(
            "INSERT OR REPLACE INTO artifact_revisions \
               (artifact_id, org, revision, title, description, category, bytes, is_bundle, entry, body_sha256) \
             SELECT id, org, revision, title, description, category, bytes, is_bundle, entry, body_sha256 \
             FROM artifacts WHERE id = '{}'; \
             UPDATE artifacts SET body_sha256 = '{}', revision = 2 WHERE id = '{}'",
            meta.id.0,
            sha256_hex(NEW),
            meta.id.0
        ));
    }
    let a2_staging = format!(".{}.staging-aaaaaaaaaaaa", a2.id.0);
    let a3_staging = format!(".{}.staging-bbbbbbbbbbbb", a3.id.0);
    std::fs::write(fixture.artifact_dir.join(&a2_staging), NEW).expect("write valid staging");
    std::fs::write(fixture.artifact_dir.join(&a3_staging), "").expect("write torn staging");

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");
    assert_eq!(
        report.transient_paths.len() as u64,
        number(node_report, "transientPaths")
    );
    assert_eq!(
        report.recovered_paths.len() as u64,
        number(node_report, "recoveredPaths")
    );
    assert_eq!(
        report.divergent_bodies.len() as u64,
        number(node_report, "divergentBodies")
    );
    assert_eq!(
        report.recovered_paths.contains(&a2_staging),
        node_report.get("a2Recovered") == Some(&Value::Bool(true))
    );
    assert_eq!(
        report.transient_paths.contains(&a3_staging),
        node_report.get("a3Transient") == Some(&Value::Bool(true))
    );
    assert_eq!(
        report.recovered_paths.contains(&a3_staging),
        node_report.get("a3Recovered") == Some(&Value::Bool(true))
    );
    assert_eq!(
        report.divergent_bodies.contains(&a3.id.0),
        node_report.get("a3Divergent") == Some(&Value::Bool(true))
    );

    assert_eq!(
        fixture.body_on_disk(&a2).as_deref(),
        Some(text(&response, "a2Live").as_str())
    );
    assert_eq!(
        fixture.history_body(&a2, 1).as_deref(),
        Some(text(&response, "a2History").as_str())
    );
    assert_eq!(
        fixture.body_on_disk(&a3).as_deref(),
        Some(text(&response, "a3Live").as_str())
    );
    assert_eq!(
        std::fs::read_to_string(fixture.artifact_dir.join(&a3_staging))
            .expect("read preserved torn staging"),
        text(&response, "a3Staging")
    );

    let current = fixture.reload(&a2).expect("recovered row");
    let restored = fixture
        .store
        .restore_for(&current, 1, None)
        .await
        .expect("revision 1 restores");
    let node_restore = field(&response, "a2Restore");
    assert_eq!(restored.meta.revision, number(node_restore, "revision"));
    assert_eq!(restored.restored_from, number(node_restore, "restoredFrom"));
    assert_eq!(
        fixture.body_on_disk(&a2).as_deref(),
        Some(text(node_restore, "body").as_str())
    );
}

// ---------------------------------------------------------------------------
// digest backfill
// ---------------------------------------------------------------------------

#[tokio::test]
async fn digest_backfill_matches_the_node_oracle() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let request = serde_json::json!({ "op": "backfill", "old": OLD, "next": NEW }).to_string();
    let response = run_node(&root, &request);

    let fixture = Fixture::new("parity-backfill");
    let single = fixture.publish_single(OLD).await;
    let bundle = fixture
        .publish_bundle(&[("index.html", NEW)], None)
        .await
        .meta;
    fixture.execute("UPDATE artifacts SET body_sha256 = ''");

    let first = fixture
        .store
        .backfill_body_digests()
        .await
        .expect("first backfill");
    let second = fixture
        .store
        .backfill_body_digests()
        .await
        .expect("second backfill");

    let node_first = field(&response, "first");
    let node_second = field(&response, "second");
    assert_eq!(first.scanned as u64, number(node_first, "scanned"));
    assert_eq!(first.updated as u64, number(node_first, "updated"));
    assert_eq!(second.scanned as u64, number(node_second, "scanned"));
    assert_eq!(second.updated as u64, number(node_second, "updated"));

    assert_eq!(
        fixture.recorded_digest(&single),
        text(&response, "singleDigest")
    );
    assert_eq!(
        fixture.recorded_digest(&bundle),
        text(&response, "bundleDigest")
    );
    assert_eq!(
        fixture.recorded_digest(&bundle),
        bundle_manifest_digest(&[("index.html".to_owned(), NEW.to_owned())]),
        "the bundle manifest digest is computed the same way on both sides"
    );
    assert_eq!(
        fixture.reload(&single).map(|row| row.revision),
        Some(number(&response, "revision"))
    );
    assert_eq!(
        field(&response, "updatedAtUnchanged"),
        &Value::Bool(true),
        "the reference also leaves updated_at alone"
    );
}

// ---------------------------------------------------------------------------
// order preservation, proved without Node
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bundle_files_are_never_round_tripped_through_a_sorted_container() {
    // Contract delta 4 in one assertion: a sorted container would answer "a.html".
    let fixture = Fixture::new("order-preservation");
    let published = fixture
        .publish_bundle(&[("z.html", "Z"), ("a.html", "A")], None)
        .await;
    assert_eq!(published.meta.entry, "z.html");

    // The same holds through an update, where `preferEntry` only applies when it still exists.
    let updated = fixture
        .store
        .update_for(
            &published.meta,
            ArtifactUpdate {
                expected_revision: 1,
                content: Some(bundle_content(&[("y.html", "Y"), ("b.html", "B")], None)),
                ..ArtifactUpdate::default()
            },
        )
        .await
        .expect("update succeeds");
    assert_eq!(updated.meta.entry, "y.html");
}
