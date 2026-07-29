//! U11 cross-runtime proof: `lib/shares.js` and `lib/feedback.js` are the oracle.
//!
//! A Rust-to-Rust round trip proves nothing about the rules this unit ports, because every one of
//! them is a *JavaScript* semantic that a reasonable Rust implementation gets subtly wrong:
//!
//! * `new Date("2027-02-31")` silently rolls over into March, and `Date` accepts hour 24 but not
//!   hour 25 or offset `+24:00`;
//! * `String.prototype.trim` strips `U+FEFF` and leaves `U+0085` alone — Rust's `trim` is the
//!   exact opposite on both;
//! * `String.prototype.length` counts UTF-16 code units, so `FEEDBACK_MAX_BODY` is not a
//!   character count;
//! * `path.posix.normalize` preserves a trailing slash;
//! * SQLite's `ORDER BY (resolved_at IS NOT NULL), …` ordering, which later conformance compares
//!   as an ordered JSON array.
//!
//! So every assertion here drives the real Node modules through `node -e`, against the *same*
//! SQLite file the Rust code then opens. `Date.now` is stubbed to a fixed instant inside the
//! driver so `"24h"` is exactly comparable rather than racy.
//!
//! # Skip visibility
//!
//! These tests **skip** when `node` or `node_modules/better-sqlite3` is unavailable, so
//! `cargo test` still works in a Rust-only environment. Per the U01 contract (§"RESOLVED at M2"),
//! `REQUIRE_NODE_REFERENCE=1` converts every skip into a hard failure, which is how CI must run:
//!
//! ```text
//! REQUIRE_NODE_REFERENCE=1 cargo test
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use artifact_mcp::config::FixedClock;
use artifact_mcp::error::AppError;
use artifact_mcp::model::{
    ArtifactId, EmailAddress, FeedbackAnchor, FeedbackId, OrgId, ShareToken, SubmitFeedback,
};
use artifact_mcp::persistence::db::{self, Database};
use artifact_mcp::persistence::feedback::{self, NewFeedback};
use artifact_mcp::persistence::shares;
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::u03_support::TempDataDir;

const REQUIRE_NODE_REFERENCE: &str = "REQUIRE_NODE_REFERENCE";

/// The instant the driver pins `Date.now` to: 2026-06-01T00:00:00.000Z.
const FIXED_NOW_MILLIS: i64 = 1_780_272_000_000;

const ORG: &str = "acme";
const CLIENT: &str = "key-acme";
const VIEWER: &str = "viewer@example.com";
const MAX_BODY: u64 = 4_000;

/// Artifacts the driver seeds. `A_RUST` exists so the Rust half can insert without disturbing the
/// rows whose ordering is being compared.
const A_SHARES: &str = "parity000001";
const A_FEEDBACK: &str = "parity000002";
const A_OTHER: &str = "parity000003";
const A_RUST: &str = "parity000004";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn require_node_reference() -> bool {
    matches!(std::env::var(REQUIRE_NODE_REFERENCE).as_deref(), Ok("1"))
}

/// Node reference availability; a missing reference is a skip unless CI demanded the proof.
fn node_reference_available(root: &Path) -> bool {
    let missing = if !root.join("node_modules/better-sqlite3").is_dir() {
        Some("node_modules/better-sqlite3 is missing")
    } else if !root.join("lib/shares.js").is_file() || !root.join("lib/feedback.js").is_file() {
        Some("lib/shares.js or lib/feedback.js is missing")
    } else {
        match Command::new("node").arg("--version").output() {
            Ok(output) if output.status.success() => None,
            _ => Some("node is not on PATH"),
        }
    };

    match missing {
        None => true,
        Some(reason) => {
            assert!(
                !require_node_reference(),
                "{REQUIRE_NODE_REFERENCE}=1 but the Node reference is unavailable ({reason}); \
                 the U11 share/feedback parity proof did not run"
            );
            eprintln!("skipping U11 Node parity proof: {reason}");
            eprintln!("set {REQUIRE_NODE_REFERENCE}=1 to make this a failure instead");
            false
        }
    }
}

/// Drives `lib/shares.js`, `lib/feedback.js`, and the two Node built-ins the anchor-page rules
/// delegate to, against the database in `DATA_DIR`.
///
/// `Date.now` is stubbed *before* `lib/shares.js` is imported so `expiryFor`'s `"24h"` branch and
/// its futureness comparison are both deterministic.
const NODE_DRIVER: &str = r##"
(async () => {
  const input = JSON.parse(process.argv[2]);
  Date.now = () => input.now;
  const base = process.argv[1];
  const path = await import("node:path");
  const db = (await import(base + "/lib/db.js")).default;
  const shares = await import(base + "/lib/shares.js");
  const feedback = await import(base + "/lib/feedback.js");

  const seed = db.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES (?, ?, ?, ?)");
  for (const id of input.artifacts) seed.run(id, input.client, input.org, "Artifact " + id);

  // 1. expiryFor, reached the way production reaches it.
  const expiry = input.expiries.map((value) => {
    try {
      const created = shares.create({ artifactId: input.expiryArtifact, org: input.org, createdBy: "seed", expires: value });
      return { input: value, ok: true, expires_at: created.expires_at };
    } catch (error) {
      return { input: value, ok: false, error: String(error && error.message) };
    }
  });

  // 2. Share rows in every state, with created_at pinned so ORDER BY is comparable.
  const madeShares = {};
  for (const [label, spec] of Object.entries(input.shares)) {
    const created = shares.create({ artifactId: input.shareArtifact, org: input.org, createdBy: spec.createdBy, expires: spec.expires });
    madeShares[label] = created.token;
    if (spec.createdAt) db.prepare("UPDATE artifact_shares SET created_at = ? WHERE token = ?").run(spec.createdAt, created.token);
    if (spec.revoke) shares.revoke(input.shareArtifact, created.token);
    if (spec.expireAt) db.prepare("UPDATE artifact_shares SET expires_at = ? WHERE token = ?").run(spec.expireAt, created.token);
  }
  const shareList = shares.listForArtifact(input.shareArtifact);
  const shareResolve = Object.entries(madeShares).map(([label, token]) => ({ label, token, grant: shares.resolve(token) }));
  shareResolve.push({ label: "unissued", token: input.unissuedToken, grant: shares.resolve(input.unissuedToken) });

  // 3. addFeedback: error strings, evaluation order, and the persisted columns.
  const parents = {};
  parents.PARENT = feedback.addFeedback({
    artifactId: input.feedbackArtifact, org: input.org, viewerEmail: input.viewer,
    body: "seed parent", artifactRevision: 1, anchor: null, anchorPage: null, parentId: null
  }).id;
  parents.REPLY = feedback.addFeedback({
    artifactId: input.feedbackArtifact, org: input.org, viewerEmail: input.viewer,
    body: "seed reply", artifactRevision: 1, anchor: null, anchorPage: null, parentId: parents.PARENT
  }).id;
  parents.FOREIGN = feedback.addFeedback({
    artifactId: input.otherArtifact, org: input.org, viewerEmail: input.viewer,
    body: "seed foreign", artifactRevision: 1, anchor: null, anchorPage: null, parentId: null
  }).id;

  const cases = input.feedbackCases.map((item) => {
    const parentId = item.parentId == null ? null : (parents[item.parentId] ?? item.parentId);
    try {
      const row = feedback.addFeedback({
        artifactId: input.rustArtifact, org: input.org, viewerEmail: input.viewer,
        body: item.body, artifactRevision: item.revision ?? 1,
        anchor: item.anchor ?? null, anchorPage: item.anchorPage ?? null, parentId
      });
      return {
        label: item.label, ok: true,
        row: {
          body: row.body, parent_id: row.parent_id, artifact_revision: row.artifact_revision,
          anchor_path: row.anchor_path, anchor_x: row.anchor_x, anchor_y: row.anchor_y,
          anchor_w: row.anchor_w, anchor_h: row.anchor_h, anchor_approx: row.anchor_approx,
          anchor_page: row.anchor_page
        }
      };
    } catch (error) {
      return { label: item.label, ok: false, error: String(error && error.message) };
    }
  });

  // 4. Listing order over rows whose timestamps are pinned.
  for (const [id, spec] of Object.entries(input.feedbackTimes)) {
    const real = parents[id] ?? id;
    db.prepare("UPDATE feedback SET created_at = ?, resolved_at = ?, resolved_by = ? WHERE id = ?")
      .run(spec.createdAt, spec.resolvedAt ?? null, spec.resolvedAt ? "agent:seed" : null, real);
  }
  const ids = (rows) => rows.map((row) => row.id);

  // 5. The two Node built-ins `validateAnchorPage` is made of.
  const normalize = input.anchorPages.map((value) => ({
    input: value,
    output: path.posix.normalize(String(value).trim().replace(/\\/g, "/"))
  }));
  const strings = input.strings.map((value) => ({ input: value, trimmed: value.trim(), length: value.trim().length }));

  process.stdout.write(JSON.stringify({
    now: input.now,
    expiry,
    shareTokens: madeShares,
    shareList,
    shareResolve,
    feedbackParents: parents,
    feedbackCases: cases,
    listForArtifact: ids(feedback.listForArtifact(input.feedbackArtifact)),
    listAll: ids(feedback.listAll()),
    listForClientOrg: ids(feedback.listForClient(input.client, undefined, input.org)),
    listForClientAdmin: ids(feedback.listForClient(input.client, undefined, null)),
    listForClientArtifact: ids(feedback.listForClient(input.client, input.feedbackArtifact, input.org)),
    normalize,
    strings
  }));
})().catch((error) => { console.error(error); process.exit(1); });
"##;

fn run_node(root: &Path, data_dir: &Path, request: &Value) -> Value {
    let base = format!("file://{}", root.display());
    let output = Command::new("node")
        .current_dir(root)
        .arg("-e")
        .arg(NODE_DRIVER)
        .arg(&base)
        .arg(request.to_string())
        .env("DATA_DIR", data_dir)
        .env("TZ", "UTC")
        .env_remove("WEBHOOK_ENC_KEY")
        .env_remove("ARTIFACT_API_KEYS")
        .env_remove("ORG_EMAIL_DOMAINS")
        .env_remove("FEEDBACK_MAX_BODY")
        .output()
        .expect("run the node shares/feedback reference");
    assert!(
        output.status.success(),
        "node reference failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("node reference emitted JSON")
}

/// Every `expires` shape the port has to agree on, including the ones only `new Date` can judge.
fn expiry_matrix() -> Vec<&'static str> {
    vec![
        "never",
        "24h",
        "",
        "24H",
        "Never",
        "tomorrow",
        "2027-1-1",
        "20270101",
        "2027-01-01T10",
        "2027-01-01 10:00Z",
        "2027-01-01T10:00:00.1234Z",
        "+2027-01-01",
        "2020-01-01",
        "2026-06-01",
        "2027-01-01",
        "2026-13-01",
        "2026-00-10",
        "2027-01-32",
        "2027-01-00",
        "2027-02-31",
        "2027-02-30",
        "2027-04-31",
        "2028-02-29",
        "2027-02-28",
        "2027-02-31T00:00Z",
        "2027-05-01T24:00Z",
        "2027-05-01T24:01Z",
        "2027-05-01T25:00Z",
        "2027-05-01T10:60Z",
        "2027-05-01T10:00:60Z",
        "2027-05-01T10:00+24:00",
        "2027-05-01T10:00+00:60",
        "2027-05-01T10:00+23:59",
        "2027-05-01T10:00-23:59",
        "2027-05-01T10:00+02:00",
        "2027-05-01T10:00-0230",
        "2027-05-01T10:00:00.5Z",
        "2027-05-01T10:00:00.05Z",
        "2027-05-01T10:00:00.123Z",
        "2027-05-01T10:00Z",
        "2027-05-01T10:00:30Z",
    ]
}

/// Anchor-page inputs that survive the traversal guard, so only normalization is left to compare.
fn anchor_page_matrix() -> Vec<&'static str> {
    vec![
        "index.html",
        "  index.html  ",
        "./index.html",
        "docs/guide.html",
        "docs//guide.html",
        "docs/./guide.html",
        "docs\\guide.html",
        "docs/guide.html/",
        "a/b/c/../..",
        ".",
        "./",
        "././",
        "a//",
        "a/./b/",
    ]
}

/// Strings whose JavaScript trim and length differ from Rust's.
fn string_matrix() -> Vec<String> {
    vec![
        "  hello  ".to_owned(),
        "\t\r\n hello \t".to_owned(),
        "\u{a0}hello\u{a0}".to_owned(),
        "\u{feff}hello\u{feff}".to_owned(),
        "\u{85}hello\u{85}".to_owned(),
        "\u{2028}hello\u{2029}".to_owned(),
        "\u{3000}hello\u{3000}".to_owned(),
        "\u{1f600}\u{1f600}".to_owned(),
        "caf\u{e9}".to_owned(),
        "e\u{301}".to_owned(),
        "   ".to_owned(),
        "\u{feff}".to_owned(),
    ]
}

/// The `addFeedback` matrix, defined once and executed by both runtimes.
fn feedback_cases() -> Vec<Value> {
    vec![
        json!({"label": "plain", "body": "  hello  "}),
        json!({"label": "empty", "body": "   "}),
        json!({"label": "zwnbsp-only", "body": "\u{feff}"}),
        json!({"label": "nel-only", "body": "\u{85}"}),
        json!({"label": "too-long", "body": "x".repeat(4001)}),
        json!({"label": "at-limit", "body": "x".repeat(4000)}),
        json!({"label": "emoji", "body": "\u{1f600}\u{1f600}"}),
        json!({"label": "revision-zero", "body": "zero revision", "revision": 0}),
        json!({"label": "reply", "body": "a reply", "parentId": "PARENT"}),
        json!({"label": "reply-to-reply", "body": "nested", "parentId": "REPLY"}),
        json!({"label": "reply-foreign", "body": "cross artifact", "parentId": "FOREIGN"}),
        json!({"label": "reply-missing", "body": "orphan", "parentId": "not-a-real-id"}),
        json!({"label": "reply-empty-parent", "body": "still top level", "parentId": ""}),
        json!({"label": "anchor-point", "body": "point", "anchor": {"x": 0.25, "y": 0.75}}),
        json!({"label": "anchor-approx", "body": "approx", "anchor": {"x": 0.25, "y": 0.75, "approx": true}}),
        json!({"label": "anchor-path", "body": "path", "anchor": {"x": 0.1, "y": 0.2, "path": "#s"}}),
        json!({"label": "anchor-long-path", "body": "long path", "anchor": {"x": 0.1, "y": 0.2, "path": "s".repeat(600)}}),
        json!({"label": "anchor-x-low", "body": "bad", "anchor": {"x": -0.1, "y": 0.5}}),
        json!({"label": "anchor-x-high", "body": "bad", "anchor": {"x": 1.1, "y": 0.5}}),
        json!({"label": "anchor-y-high", "body": "bad", "anchor": {"x": 0.5, "y": 1.1}}),
        json!({"label": "anchor-edges", "body": "edges", "anchor": {"x": 0.0, "y": 1.0}}),
        json!({"label": "box-w-only", "body": "bad", "anchor": {"x": 0.1, "y": 0.1, "w": 0.2}}),
        json!({"label": "box-h-only", "body": "bad", "anchor": {"x": 0.1, "y": 0.1, "h": 0.2}}),
        json!({"label": "box-w-high", "body": "bad", "anchor": {"x": 0.1, "y": 0.1, "w": 1.5, "h": 0.2}}),
        json!({"label": "box-w-negative", "body": "bad", "anchor": {"x": 0.1, "y": 0.1, "w": -0.5, "h": 0.2}}),
        json!({"label": "box-zero", "body": "bad", "anchor": {"x": 0.1, "y": 0.1, "w": 0.0, "h": 0.2}}),
        json!({"label": "box-trimmed", "body": "trimmed", "anchor": {"x": 0.8, "y": 0.9, "w": 0.5, "h": 0.5}}),
        json!({"label": "box-edge-start", "body": "bad", "anchor": {"x": 1.0, "y": 0.5, "w": 0.2, "h": 0.2}}),
        json!({"label": "box-full", "body": "full", "anchor": {"x": 0.0, "y": 0.0, "w": 1.0, "h": 1.0}}),
        json!({"label": "page-with-anchor", "body": "paged", "anchor": {"x": 0.5, "y": 0.5}, "anchorPage": "docs/guide.html"}),
        json!({"label": "page-without-anchor", "body": "no anchor", "anchorPage": "docs/guide.html"}),
        json!({"label": "page-on-reply", "body": "reply page", "parentId": "PARENT", "anchor": {"x": 0.5, "y": 0.5}, "anchorPage": "docs/guide.html"}),
        json!({"label": "empty-and-bad-anchor", "body": "  ", "anchor": {"x": 9.0, "y": 9.0}}),
        json!({"label": "bad-parent-and-bad-anchor", "body": "ok", "parentId": "not-a-real-id", "anchor": {"x": 9.0, "y": 9.0}}),
    ]
}

/// Rebuilds one matrix case as the typed Rust submission.
fn rust_submission(
    case: &Value,
    parents: &dyn Fn(&str) -> String,
) -> (SubmitFeedback, Option<f64>) {
    let anchor_value = case.get("anchor");
    let number = |name: &str| {
        anchor_value
            .and_then(|anchor| anchor.get(name))
            .and_then(Value::as_f64)
    };
    let anchor = anchor_value.map(|anchor| FeedbackAnchor {
        x: number("x").unwrap_or(f64::NAN),
        y: number("y").unwrap_or(f64::NAN),
        w: number("w"),
        h: number("h"),
        approx: anchor
            .get("approx")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    });
    let submission = SubmitFeedback {
        viewer_email: EmailAddress::from(VIEWER),
        body: case["body"].as_str().expect("case body").to_owned(),
        parent_id: case
            .get("parentId")
            .and_then(Value::as_str)
            .map(|parent| FeedbackId(parents(parent))),
        anchor,
        anchor_path: anchor_value
            .and_then(|anchor| anchor.get("path"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        anchor_page: None,
    };
    (
        submission,
        case.get("revision").and_then(Value::as_f64).or(Some(1.0)),
    )
}

/// SQLite REAL columns come back from better-sqlite3 as JavaScript numbers, so an exact `0.0`
/// serializes as `0` on one side and `0.0` on the other; compare the values, not their spelling.
fn assert_number(rust: Option<f64>, node: &Value, label: &str, field: &str) {
    match (rust, node.as_f64()) {
        (None, None) => assert!(node.is_null(), "{label} {field}: node had {node}"),
        (Some(rust), Some(node)) => assert!(
            (rust - node).abs() < 1e-12,
            "{label} {field}: rust {rust} vs node {node}"
        ),
        (rust, _) => panic!("{label} {field}: rust {rust:?} vs node {node}"),
    }
}

#[test]
fn rust_matches_the_node_share_and_feedback_reference() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let data_dir = TempDataDir::new("u11-parity");
    let request = json!({
        "now": FIXED_NOW_MILLIS,
        "org": ORG,
        "client": CLIENT,
        "viewer": VIEWER,
        "artifacts": [A_SHARES, A_FEEDBACK, A_OTHER, A_RUST, "parity000005"],
        "expiryArtifact": "parity000005",
        "shareArtifact": A_SHARES,
        "feedbackArtifact": A_FEEDBACK,
        "otherArtifact": A_OTHER,
        "rustArtifact": A_RUST,
        "unissuedToken": "AAAAAAAAAAAAAAAAAAAAAAAA",
        "expiries": expiry_matrix(),
        "anchorPages": anchor_page_matrix(),
        "strings": string_matrix(),
        "feedbackCases": feedback_cases(),
        "shares": {
            "live": { "expires": "never", "createdBy": "viewer@example.com", "createdAt": "2026-05-01 00:00:00" },
            "later": { "expires": "never", "createdBy": "agent:key-acme", "createdAt": "2026-05-02 00:00:00" },
            "tied": { "expires": "never", "createdBy": "seed", "createdAt": "2026-05-01 00:00:00" },
            "dated": { "expires": "2027-01-01", "createdBy": "seed", "createdAt": "2026-04-01 00:00:00" },
            "revoked": { "expires": "never", "createdBy": "seed", "createdAt": "2026-05-03 00:00:00", "revoke": true },
            "expired": { "expires": "never", "createdBy": "seed", "createdAt": "2026-05-04 00:00:00", "expireAt": "2020-01-01T00:00:00.000Z" }
        },
        "feedbackTimes": {
            "PARENT": { "createdAt": "2026-01-01 00:00:00" },
            "REPLY": { "createdAt": "2026-03-01 00:00:00", "resolvedAt": "2026-04-01 00:00:00" },
            "FOREIGN": { "createdAt": "2026-02-01 00:00:00" }
        }
    });

    let node = run_node(&root, data_dir.path(), &request);

    // Open the *same* file the Node reference just wrote and migrated.
    let pool = Database::open_at(data_dir.path()).expect("open the node-created database");
    let conn = db::checkout(&pool).expect("check out a connection");
    let clock = FixedClock::from_millis(FIXED_NOW_MILLIS);
    let org_id = OrgId::from(ORG);

    // ---------------------------------------------------------------- expiry
    let expiry = node["expiry"].as_array().expect("expiry results");
    assert_eq!(expiry.len(), expiry_matrix().len());
    // The driver's `Date.now` stub really took effect, so `"24h"` is a fixed instant and the
    // futureness comparisons below are reproducible rather than racy.
    assert_eq!(
        expiry[1]["expires_at"], "2026-06-02T00:00:00.000Z",
        "Date.now was not pinned inside the node driver"
    );
    let accepted = expiry
        .iter()
        .filter(|outcome| outcome["ok"].as_bool() == Some(true))
        .count();
    assert!(
        accepted > 10 && accepted < expiry.len(),
        "the expiry matrix must exercise both outcomes, got {accepted} accepted"
    );
    for outcome in expiry {
        let input = outcome["input"].as_str().expect("input");
        let rust = shares::expiry_for(&clock, input);
        if outcome["ok"].as_bool() == Some(true) {
            let expected = outcome["expires_at"].as_str().map(str::to_owned);
            assert_eq!(
                rust,
                Ok(expected.clone()),
                "expires {input:?}: node accepted it as {expected:?}"
            );
        } else {
            let message = outcome["error"].as_str().expect("error message");
            assert_eq!(
                rust,
                Err(AppError::Validation(message.to_owned())),
                "expires {input:?}: node rejected it with {message:?}"
            );
        }
    }

    // ---------------------------------------------------------------- shares
    // Every token Node minted, resolved by Rust against Node's own rows.
    for outcome in node["shareResolve"].as_array().expect("resolve results") {
        let token = ShareToken::from(outcome["token"].as_str().expect("token"));
        let label = outcome["label"].as_str().expect("label");
        let rust = shares::resolve(&conn, &token).expect("resolve");
        let node_grant = &outcome["grant"];
        if node_grant.is_null() {
            assert_eq!(rust, None, "{label} must not resolve in Rust either");
        } else {
            let grant = rust.unwrap_or_else(|| panic!("{label} should resolve in Rust"));
            assert_eq!(grant.artifact_id.0, node_grant["artifact_id"]);
            assert_eq!(grant.org.0, node_grant["org"]);
        }
    }
    // …and the revoked/expired ones are exactly the tokens Node also refuses.
    let mut refused: Vec<&str> = node["shareResolve"]
        .as_array()
        .expect("resolve results")
        .iter()
        .filter(|outcome| outcome["grant"].is_null())
        .map(|outcome| outcome["label"].as_str().expect("label"))
        .collect();
    refused.sort_unstable();
    assert_eq!(
        refused,
        ["expired", "revoked", "unissued"],
        "exactly the revoked, expired, and never-issued tokens are refused"
    );

    let node_list: Vec<Value> = node["shareList"]
        .as_array()
        .expect("share list")
        .iter()
        .map(|row| {
            json!({
                "token": row["token"],
                "expires_at": row["expires_at"],
                "created_at": row["created_at"],
                "created_by": row["created_by"],
            })
        })
        .collect();
    let rust_list: Vec<Value> = shares::list_for_artifact(&conn, &ArtifactId::from(A_SHARES))
        .expect("list shares")
        .into_iter()
        .map(|share| {
            json!({
                "token": share.token.0,
                "expires_at": share.expires_at.map(|value| value.0),
                "created_at": share.created_at.map(|value| value.0),
                "created_by": share.created_by,
            })
        })
        .collect();
    assert_eq!(
        rust_list, node_list,
        "share listing must match Node row for row, in order"
    );
    assert_eq!(rust_list.len(), 4, "revoked and expired rows stay hidden");

    // -------------------------------------------------------------- feedback
    // Node created its rows; Rust re-runs the same matrix against its own artifact and must
    // report the same acceptance, the same messages, and the same persisted columns.
    let parents = node["feedbackParents"].clone();
    let resolve_parent = |placeholder: &str| {
        parents
            .get(placeholder)
            .and_then(Value::as_str)
            .unwrap_or(placeholder)
            .to_owned()
    };
    let rust_artifact = ArtifactId::from(A_RUST);
    let ids = artifact_mcp::config::SequentialIdSource::default();
    let node_cases = node["feedbackCases"].as_array().expect("feedback cases");
    assert_eq!(node_cases.len(), feedback_cases().len());
    let accepted = node_cases
        .iter()
        .filter(|case| case["ok"].as_bool() == Some(true))
        .count();
    assert!(
        accepted > 10 && accepted < node_cases.len(),
        "the feedback matrix must exercise both outcomes, got {accepted} accepted"
    );
    for (case, expected) in feedback_cases().iter().zip(node_cases) {
        let label = case["label"].as_str().expect("label");
        assert_eq!(label, expected["label"], "case order must line up");
        let (submission, revision) = rust_submission(case, &resolve_parent);
        let result = feedback::add(
            &conn,
            &ids,
            &NewFeedback {
                artifact_id: &rust_artifact,
                org: &org_id,
                #[allow(
                    clippy::cast_sign_loss,
                    clippy::cast_possible_truncation,
                    reason = "matrix revisions are small non-negative integers"
                )]
                artifact_revision: revision.unwrap_or(1.0) as u64,
                submission: &submission,
                anchor_page: case.get("anchorPage").and_then(Value::as_str),
                max_body: MAX_BODY,
            },
        );

        if expected["ok"].as_bool() == Some(true) {
            let row =
                result.unwrap_or_else(|error| panic!("{label}: node accepted, Rust said {error}"));
            let node_row = &expected["row"];
            assert_eq!(row.body, node_row["body"], "{label} body");
            assert_eq!(
                row.parent_id.is_some(),
                !node_row["parent_id"].is_null(),
                "{label} parent"
            );
            assert_eq!(
                i64::try_from(row.artifact_revision).expect("revision fits"),
                node_row["artifact_revision"].as_i64().expect("revision"),
                "{label} revision"
            );
            assert_eq!(
                json!(row.anchor_path),
                node_row["anchor_path"],
                "{label} path"
            );
            // Compared numerically: better-sqlite3 hands back `0` where Rust has `0.0`, and
            // `serde_json` does not consider those the same token.
            assert_number(row.anchor_x, &node_row["anchor_x"], label, "x");
            assert_number(row.anchor_y, &node_row["anchor_y"], label, "y");
            assert_number(row.anchor_w, &node_row["anchor_w"], label, "w");
            assert_number(row.anchor_h, &node_row["anchor_h"], label, "h");
            assert_eq!(
                i64::from(row.anchor_approx),
                node_row["anchor_approx"].as_i64().expect("approx flag"),
                "{label} approx"
            );
            assert_eq!(
                json!(row.anchor_page),
                node_row["anchor_page"],
                "{label} page"
            );
        } else {
            let message = expected["error"].as_str().expect("error message");
            assert_eq!(
                result,
                Err(AppError::Validation(message.to_owned())),
                "{label}: node rejected with {message:?}"
            );
        }
    }

    // ---------------------------------------------------------------- order
    let node_ids = |key: &str| -> Vec<String> {
        node[key]
            .as_array()
            .unwrap_or_else(|| panic!("{key} missing"))
            .iter()
            .map(|id| id.as_str().expect("id").to_owned())
            .collect()
    };
    let feedback_artifact = ArtifactId::from(A_FEEDBACK);
    // Node's dumps were taken before Rust inserted anything, so compare only Node's own rows.
    let node_written: Vec<String> = node_ids("listAll");
    assert!(
        node_written.len() > 10,
        "the ordering comparison covers the three pinned rows plus every accepted matrix row, \
         got {}",
        node_written.len()
    );
    let rust_ids = |rows: Vec<artifact_mcp::model::Feedback>| -> Vec<String> {
        rows.into_iter()
            .map(|row| row.id.0)
            .filter(|id| node_written.contains(id))
            .collect()
    };
    assert_eq!(
        rust_ids(feedback::list_for_artifact(&conn, &feedback_artifact).expect("list")),
        node_ids("listForArtifact"),
        "thread order"
    );
    assert_eq!(
        rust_ids(feedback::list_all(&conn, None).expect("list all")),
        node_written,
        "firehose order"
    );
    assert_eq!(
        rust_ids(
            feedback::list_for_client(
                &conn,
                &artifact_mcp::model::ClientId::from(CLIENT),
                None,
                Some(&org_id)
            )
            .expect("client listing")
        ),
        node_ids("listForClientOrg"),
        "agent order, org-scoped"
    );
    assert_eq!(
        rust_ids(
            feedback::list_for_client(
                &conn,
                &artifact_mcp::model::ClientId::from(CLIENT),
                None,
                None
            )
            .expect("admin listing")
        ),
        node_ids("listForClientAdmin"),
        "agent order, admin"
    );
    assert_eq!(
        rust_ids(
            feedback::list_for_client(
                &conn,
                &artifact_mcp::model::ClientId::from(CLIENT),
                Some(&feedback_artifact),
                Some(&org_id)
            )
            .expect("client+artifact listing")
        ),
        node_ids("listForClientArtifact"),
        "agent order, one artifact"
    );

    // ------------------------------------------------- JavaScript primitives
    // `path.posix.normalize`, which `validateAnchorPage` delegates its normalization to.
    let anchor = FeedbackAnchor {
        x: 0.5,
        y: 0.5,
        w: None,
        h: None,
        approx: false,
    };
    let accept_everything = |_: &str| true;
    for outcome in node["normalize"].as_array().expect("normalize results") {
        let input = outcome["input"].as_str().expect("input");
        let expected = outcome["output"].as_str().expect("output");
        let rust =
            feedback::validate_anchor_page(true, Some(&anchor), Some(input), &accept_everything);
        if expected == "." {
            // Node's route rejects a path that normalizes to nothing before it looks it up.
            assert!(matches!(rust, Err(AppError::Validation(_))), "{input:?}");
        } else if input.split('/').any(|part| part == "..") {
            // Traversal is rejected before normalization ever runs.
            assert!(matches!(rust, Err(AppError::Validation(_))), "{input:?}");
        } else {
            assert_eq!(rust, Ok(Some(expected.to_owned())), "normalize {input:?}");
        }
    }

    // `String.prototype.trim` and `.length`, which bound the feedback body.
    for outcome in node["strings"].as_array().expect("string results") {
        let input = outcome["input"].as_str().expect("input");
        assert_eq!(
            feedback::js_trim(input),
            outcome["trimmed"].as_str().expect("trimmed"),
            "trim {input:?}"
        );
        assert_eq!(
            i64::try_from(feedback::utf16_len(feedback::js_trim(input))).expect("length fits"),
            outcome["length"].as_i64().expect("length"),
            "length {input:?}"
        );
    }
}

/// The messages `validateAnchorPage` throws live in `lib/app.js`, which exports neither the
/// function nor the strings, so they cannot be driven directly. Pinning them against the source
/// text is the next best thing: this fails loudly the moment the Node wording drifts.
#[test]
fn every_ported_message_still_exists_verbatim_in_the_node_source() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }
    let read = |relative: &str| {
        std::fs::read_to_string(root.join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"))
    };

    let app = read("lib/app.js");
    for message in [
        feedback::ANCHOR_PAGE_UNANCHORED_MESSAGE,
        feedback::ANCHOR_PAGE_NOT_BUNDLE_MESSAGE,
        feedback::ANCHOR_PAGE_REQUIRED_MESSAGE,
        feedback::ANCHOR_PAGE_TRAVERSAL_MESSAGE,
        feedback::ANCHOR_PAGE_NOT_A_FILE_MESSAGE,
        feedback::ANCHOR_PAGE_MISSING_MESSAGE,
        feedback::NOT_FOUND_MESSAGE,
        feedback::FORBIDDEN_MESSAGE,
    ] {
        assert!(
            app.contains(message),
            "lib/app.js no longer contains {message:?}"
        );
    }

    let store = read("lib/feedback.js");
    for message in [
        feedback::EMPTY_BODY_MESSAGE,
        feedback::PARENT_NOT_FOUND_MESSAGE,
        feedback::PARENT_OTHER_ARTIFACT_MESSAGE,
        feedback::PARENT_NOT_TOP_LEVEL_MESSAGE,
        feedback::ANCHOR_NOT_OBJECT_MESSAGE,
        feedback::ANCHOR_POINT_MESSAGE,
        feedback::ANCHOR_BOX_PAIR_MESSAGE,
        feedback::ANCHOR_BOX_RANGE_MESSAGE,
        feedback::ANCHOR_BOX_POSITIVE_MESSAGE,
        feedback::ANCHOR_BOX_BOUNDS_MESSAGE,
    ] {
        assert!(
            store.contains(message),
            "lib/feedback.js no longer contains {message:?}"
        );
    }
    assert!(
        store.contains("Feedback is too long (max ${FEEDBACK_MAX_BODY} characters)."),
        "the too-long template changed shape"
    );

    let share_source = read("lib/shares.js");
    for message in [
        shares::EXPIRES_FORMAT_MESSAGE,
        shares::EXPIRES_FUTURE_MESSAGE,
        shares::EXPIRES_CALENDAR_MESSAGE,
    ] {
        assert!(
            share_source.contains(message),
            "lib/shares.js no longer contains {message:?}"
        );
    }
    // The SQL predicates this port copies verbatim.
    for fragment in [
        "revoked_at IS NULL AND (expires_at IS NULL OR julianday(expires_at) > julianday('now'))",
        "ORDER BY created_at DESC, token DESC",
    ] {
        assert!(
            share_source.contains(fragment),
            "lib/shares.js no longer contains {fragment:?}"
        );
    }
    for fragment in [
        "ORDER BY (resolved_at IS NOT NULL), created_at ASC, id ASC",
        "ORDER BY (f.resolved_at IS NOT NULL), f.created_at DESC, f.id DESC",
        "ORDER BY (resolved_at IS NOT NULL), created_at DESC, id DESC",
    ] {
        assert!(
            store.contains(fragment),
            "lib/feedback.js no longer orders by {fragment:?}"
        );
    }
    let _ = Connection::open_in_memory();
}
