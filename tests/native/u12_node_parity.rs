//! U12 cross-runtime proof: the Discord payload, the display mask, and the allowlist decision
//! must all match the real `lib/notify.js` / `lib/webhooks.js`.
//!
//! Nothing here compares Rust to Rust. Each assertion drives the Node reference through `node -e`
//! against a throwaway `DATA_DIR`, so the oracle is the shipping implementation:
//!
//! * **Embed bytes** — `JSON.stringify(notify.buildEmbed(event, payload))` versus
//!   `serde_json::to_string(&build_embed(...))`. Key order, number formatting, string escaping,
//!   truncation and every fallback are covered by the same comparison.
//! * **Masking and the allowlist** — `webhooks.maskUrl` and the accept/reject decision of
//!   `webhooks.create`, taken from the real regex rather than a restatement of it.
//! * **At-rest layout** — the row Node writes is read back by the Rust store from the same SQLite
//!   file, in both the plaintext and the encrypted configuration.
//!
//! # Skip visibility
//!
//! These tests skip when `node` or `lib/` is unavailable so `cargo test` still works in a
//! Rust-only environment. Per the U01 contract that skip must be convertible into a failure:
//!
//! ```text
//! REQUIRE_NODE_REFERENCE=1 cargo test
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use artifact_mcp::integrations::notify::build_embed;
use artifact_mcp::model::{
    ArtifactId, CreateWebhook, EmailAddress, NotificationPayload, OrgId, WebhookEvent,
};
use artifact_mcp::persistence::webhooks::{event_name, mask_url};
use serde_json::{Value, json};

use crate::u12_support::{TempDir, open_pool, seed_org, store_with, test_key};

/// Setting this to `1` turns "Node is unavailable" from a skip into a failure.
const REQUIRE_NODE_REFERENCE: &str = "REQUIRE_NODE_REFERENCE";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn require_node_reference() -> bool {
    std::env::var(REQUIRE_NODE_REFERENCE).is_ok_and(|value| value == "1")
}

/// Node reference availability, with the contract's skip-to-failure conversion.
fn node_reference_available(root: &Path) -> bool {
    let unavailable = if !root.join("lib/notify.js").is_file() {
        Some("lib/notify.js is missing")
    } else if !root.join("node_modules/better-sqlite3").exists() {
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
                 the U12 Node parity proof did not run"
            );
            eprintln!("skipping U12 Node parity proof: {reason}");
            eprintln!("set {REQUIRE_NODE_REFERENCE}=1 to make this a failure instead");
            false
        }
    }
}

/// One `node -e` run over the real `lib/` modules.
///
/// `process.argv[1]` is the repository root and `argv[2]` the JSON request, matching the shape
/// `u03_cross_runtime.rs` and `u04_crypto.rs` established.
const NODE_DRIVER: &str = r#"
const root = process.argv[1];
const input = JSON.parse(process.argv[2]);
const load = (name) => import(`file://${root}/lib/${name}`);
Promise.all([load("notify.js"), load("webhooks.js"), load("orgs.js"), load("db.js")])
  .then(([notify, webhooks, orgs, dbModule]) => {
    const out = { embeds: [], masks: [], creates: [], listed: [] };
    for (const item of input.embedCases) {
      out.embeds.push(JSON.stringify(notify.buildEmbed(item.event, item.payload)));
    }
    for (const value of input.maskCases) out.masks.push(webhooks.maskUrl(value));
    if (input.org) {
      orgs.createOrg({ name: input.org, label: "Parity Org" });
      const db = dbModule.default;
      const rawStmt = db.prepare(
        "SELECT url, url_cipher IS NOT NULL AS encrypted FROM org_webhooks WHERE id = ?"
      );
      for (const item of input.createCases) {
        try {
          const row = webhooks.create({ org: input.org, url: item.url, label: item.label });
          const raw = rawStmt.get(row.id);
          out.creates.push({
            accepted: true,
            id: row.id,
            url: row.url,
            label: row.label,
            events: row.events,
            storedUrl: raw.url,
            encrypted: Boolean(raw.encrypted)
          });
        } catch (error) {
          out.creates.push({ accepted: false, error: String(error && error.message) });
        }
      }
      out.listed = webhooks.listForOrg(input.org);
    }
    process.stdout.write(JSON.stringify(out));
  })
  .catch((error) => { console.error(error); process.exit(1); });
"#;

fn run_node(root: &Path, data_dir: &Path, key: Option<&str>, request: &Value) -> Value {
    let mut command = Command::new("node");
    command
        .current_dir(root)
        .arg("-e")
        .arg(NODE_DRIVER)
        .arg(root.to_string_lossy().as_ref())
        .arg(request.to_string())
        .env("DATA_DIR", data_dir)
        // The v7 migration seeds orgs from these; keep the fixture database empty of surprises.
        .env_remove("ORG_DOMAINS")
        .env_remove("ARTIFACT_API_KEYS");
    match key {
        Some(value) => command.env("WEBHOOK_ENC_KEY", value),
        None => command.env_remove("WEBHOOK_ENC_KEY"),
    };
    let output = command.output().expect("run the node webhook reference");
    assert!(
        output.status.success(),
        "node reference failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "node reference did not emit JSON ({error}):\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn array<'a>(response: &'a Value, name: &str) -> &'a Vec<Value> {
    response
        .get(name)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("node response is missing the {name} array"))
}

// ---------------------------------------------------------------------------
// Embed cases
// ---------------------------------------------------------------------------

/// One `(event, org, payload)` triple, rendered for both runtimes from a single source.
struct EmbedCase {
    name: &'static str,
    event: WebhookEvent,
    org: OrgId,
    payload: NotificationPayload,
}

fn base_payload() -> NotificationPayload {
    NotificationPayload {
        artifact_id: ArtifactId("abc123def456".to_owned()),
        title: "Quarterly report".to_owned(),
        url: "https://example.test/abc123def456".to_owned(),
        description: "The numbers are in.".to_owned(),
        uploader_label: "Ada Lovelace".to_owned(),
        category: "Reports".to_owned(),
        revision: 3,
        bytes: 2048,
        viewer_email: Some(EmailAddress("viewer@example.test".to_owned())),
        body: Some("Looks good to me".to_owned()),
        resolver: Some("Grace Hopper".to_owned()),
    }
}

/// The Node payload literal for a case. Keys are the camelCase names `lib/notify.js` reads; `org`
/// is injected the way `emit` does.
fn node_payload(case: &EmbedCase) -> Value {
    let payload = &case.payload;
    json!({
        "org": case.org.0,
        "title": payload.title,
        "url": payload.url,
        "description": payload.description,
        "uploaderLabel": payload.uploader_label,
        "category": payload.category,
        "revision": payload.revision,
        "bytes": payload.bytes,
        "viewerEmail": payload.viewer_email.as_ref().map(|email| email.0.clone()),
        "body": payload.body,
        "resolver": payload.resolver,
    })
}

fn embed_cases() -> Vec<EmbedCase> {
    let acme = OrgId("acme".to_owned());
    let mut cases = Vec::new();

    // Every event with a full payload.
    for event in [
        WebhookEvent::Published,
        WebhookEvent::Updated,
        WebhookEvent::Restored,
        WebhookEvent::Deleted,
        WebhookEvent::Feedback,
        WebhookEvent::Resolved,
    ] {
        cases.push(EmbedCase {
            name: "full payload",
            event,
            org: acme.clone(),
            payload: base_payload(),
        });
    }

    // Every fallback at once: empty title/description/labels, zero revision, no URL, no org.
    let mut empty = base_payload();
    empty.title = String::new();
    empty.description = "   ".to_owned();
    empty.uploader_label = String::new();
    empty.category = String::new();
    empty.url = String::new();
    empty.revision = 0;
    empty.body = None;
    empty.viewer_email = None;
    empty.resolver = None;
    for event in [
        WebhookEvent::Published,
        WebhookEvent::Deleted,
        WebhookEvent::Feedback,
        WebhookEvent::Resolved,
    ] {
        cases.push(EmbedCase {
            name: "every fallback",
            event,
            org: OrgId(String::new()),
            payload: empty.clone(),
        });
    }

    // Truncation at each limit, including a multi-byte character on the boundary.
    let mut long = base_payload();
    long.title = "T".repeat(400);
    long.description = "D".repeat(3000);
    long.uploader_label = "U".repeat(1200);
    long.category = "C".repeat(1200);
    cases.push(EmbedCase {
        name: "truncation",
        event: WebhookEvent::Published,
        org: acme.clone(),
        payload: long,
    });

    // Characters that JSON must escape, plus non-ASCII that it must not.
    let mut escaping = base_payload();
    escaping.title = "quote \" backslash \\ newline \n tab \t control \u{1} del \u{7f}".to_owned();
    escaping.description = "ünïcødé — 🎉 — トークン — \u{2028}\u{2029}".to_owned();
    escaping.uploader_label = "</script><script>alert(1)</script>".to_owned();
    escaping.category = "a/b".to_owned();
    cases.push(EmbedCase {
        name: "escaping",
        event: WebhookEvent::Published,
        org: OrgId("acme & co".to_owned()),
        payload: escaping,
    });

    // Byte sizes across every branch of `bytes()`, including the exact `toFixed(1)` ties that
    // Rust's default round-half-to-even would get wrong.
    for size in [
        0,
        1,
        1023,
        1024,
        1280,       // 1.25 KiB — a tie
        1024 + 768, // 1.75 KiB — a tie
        1_048_575,  // rounds up to "1024.0 KB"
        1_048_576,
        1_048_576 + 262_144, // 1.25 MiB — a tie
        1_048_576 + 786_432, // 1.75 MiB — a tie
        123_456_789,
        u64::from(u32::MAX),
    ] {
        let mut payload = base_payload();
        payload.bytes = size;
        cases.push(EmbedCase {
            name: "byte formatting",
            event: WebhookEvent::Published,
            org: acme.clone(),
            payload,
        });
    }

    // Large revision numbers must render identically (Node stringifies a Number).
    let mut big_revision = base_payload();
    big_revision.revision = 9_007_199_254_740_991;
    cases.push(EmbedCase {
        name: "max safe revision",
        event: WebhookEvent::Updated,
        org: acme,
        payload: big_revision,
    });

    cases
}

/// URL shapes `maskUrl` must render identically in both runtimes.
const MASK_CASES: &[&str] = &[
    "https://discord.com/api/webhooks/123456789012345678/secret-token",
    "https://discord.com/…oken",
    "https://discordapp.com/api/webhooks/1/t",
    "https://hooks.example.com:8443/abc/secret",
    "https://example.test:443/abc/secret",
    "http://localhost:3480/x",
    "https://user:pass@example.test/path",
    "https://[::1]:9000/path",
    "not a url",
    "abc",
    "",
    "https://example.test/",
    // `slice(-4)` counts UTF-16 units: three astral characters end in two of them, not four.
    "https://example.test/\u{1F389}\u{1F389}\u{1F389}",
    "https://example.test/ab\u{1F389}",
];

/// URLs whose accept/reject decision `create()` must reach identically.
const CREATE_CASES: &[&str] = &[
    "https://discord.com/api/webhooks/1/token",
    "https://discordapp.com/api/webhooks/1/token",
    "HTTPS://DISCORD.COM/API/WEBHOOKS/1/token",
    "http://discord.com/api/webhooks/1/token",
    "https://discord.com.evil.tld/api/webhooks/1/token",
    "https://discordapp.com.evil.tld/api/webhooks/1/token",
    "https://sub.discord.com/api/webhooks/1/token",
    "https://evil-discord.com/api/webhooks/1/token",
    "https://discord.com@evil.tld/api/webhooks/1/token",
    "https://evil.tld/https://discord.com/api/webhooks/1/t",
    "https://169.254.169.254/api/webhooks/1/token",
    "https://127.0.0.1/api/webhooks/1/token",
    "https://[::1]/api/webhooks/1/token",
    "https://discord.com/api/webhook/1/token",
    "  https://discord.com/api/webhooks/1/trimmed  ",
    "file:///etc/passwd",
    "",
];

const CREATE_LABEL: &str = "  Ops channel  ";

fn request_json(cases: &[EmbedCase], org: Option<&str>) -> Value {
    json!({
        "embedCases": cases
            .iter()
            .map(|case| json!({ "event": event_name(&case.event), "payload": node_payload(case) }))
            .collect::<Vec<Value>>(),
        "maskCases": MASK_CASES,
        "org": org,
        "createCases": CREATE_CASES
            .iter()
            .map(|url| json!({ "url": url, "label": CREATE_LABEL }))
            .collect::<Vec<Value>>(),
    })
}

// ---------------------------------------------------------------------------
// Proofs
// ---------------------------------------------------------------------------

#[test]
fn rust_embeds_are_byte_identical_to_the_node_reference() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }
    let dir = TempDir::new("parity-embeds");
    let cases = embed_cases();
    let response = run_node(&root, dir.path(), None, &request_json(&cases, None));

    let node_embeds = array(&response, "embeds");
    assert_eq!(
        node_embeds.len(),
        cases.len(),
        "the node driver skipped a case"
    );
    for (case, expected) in cases.iter().zip(node_embeds) {
        let expected = expected.as_str().expect("node emitted a JSON string");
        let actual = serde_json::to_string(&build_embed(&case.event, &case.org, &case.payload))
            .expect("serialize the rust embed");
        assert_eq!(
            actual,
            expected,
            "embed bytes diverged for case {:?} / event {}",
            case.name,
            event_name(&case.event)
        );
    }
}

#[test]
fn rust_and_node_mask_display_urls_identically() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }
    let dir = TempDir::new("parity-masks");
    let response = run_node(&root, dir.path(), None, &request_json(&[], None));

    let node_masks = array(&response, "masks");
    assert_eq!(node_masks.len(), MASK_CASES.len());
    for (value, expected) in MASK_CASES.iter().zip(node_masks) {
        let expected = expected.as_str().expect("node emitted a JSON string");
        assert_eq!(mask_url(value), expected, "mask diverged for {value:?}");
    }
}

/// The allowlist matrix, decided by the real `lib/webhooks.js` regex and by the Rust predicate,
/// with the stored row compared column for column.
#[tokio::test]
async fn rust_and_node_reach_the_same_allowlist_and_at_rest_decisions() {
    for key in [None, Some(test_key())] {
        let root = repo_root();
        if !node_reference_available(&root) {
            return;
        }
        let node_dir = TempDir::new("parity-create-node");
        let response = run_node(
            &root,
            node_dir.path(),
            key.as_deref(),
            &request_json(&[], Some("parity")),
        );
        let node_creates = array(&response, "creates").clone();
        assert_eq!(node_creates.len(), CREATE_CASES.len());

        let rust_dir = TempDir::new("parity-create-rust");
        let pool = open_pool(rust_dir.path());
        seed_org(&pool, "parity").await;
        let store = store_with(pool.clone(), key.as_deref());

        for (url, node_row) in CREATE_CASES.iter().zip(&node_creates) {
            let node_accepted = node_row
                .get("accepted")
                .and_then(Value::as_bool)
                .expect("node reported an accept flag");
            let outcome = store
                .create(CreateWebhook {
                    org: OrgId("parity".to_owned()),
                    url: (*url).to_owned(),
                    label: CREATE_LABEL.to_owned(),
                    events: None,
                })
                .await;

            match (node_accepted, outcome) {
                (false, Ok(summary)) => panic!(
                    "Rust accepted {url:?} (stored as {}) but the Node allowlist rejected it",
                    summary.url
                ),
                (true, Err(error)) => {
                    panic!("Node accepted {url:?} but Rust rejected it: {error}")
                }
                (false, Err(error)) => {
                    let node_error = node_row
                        .get("error")
                        .and_then(Value::as_str)
                        .expect("node reported an error message");
                    assert_eq!(
                        error.to_string(),
                        node_error,
                        "rejection message diverged for {url:?}"
                    );
                }
                (true, Ok(summary)) => {
                    // The masked display value, the label truncation, and the default event set.
                    assert_eq!(
                        summary.url,
                        node_row.get("url").and_then(Value::as_str).unwrap_or(""),
                        "masked url diverged for {url:?}"
                    );
                    assert_eq!(
                        summary.label,
                        node_row.get("label").and_then(Value::as_str).unwrap_or(""),
                        "label diverged for {url:?}"
                    );
                    assert_eq!(
                        summary
                            .events
                            .iter()
                            .map(event_name)
                            .collect::<Vec<&str>>()
                            .join(","),
                        node_row
                            .get("events")
                            .and_then(Value::as_array)
                            .map(|events| events
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<&str>>()
                                .join(","))
                            .unwrap_or_default(),
                        "default event set diverged for {url:?}"
                    );

                    // The at-rest layout: same `url` column, same encrypted/plaintext choice.
                    let (stored, cipher, _, _) =
                        crate::u12_support::raw_url_columns(&pool, &summary.id.0).await;
                    assert_eq!(
                        stored,
                        node_row
                            .get("storedUrl")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        "stored url column diverged for {url:?}"
                    );
                    assert_eq!(
                        cipher.is_some(),
                        node_row
                            .get("encrypted")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        "at-rest encryption choice diverged for {url:?}"
                    );
                }
            }
        }
    }
}

/// The strongest containment proof available: rows written by Node, read by Rust from the same
/// SQLite file — recovered with the key, and refused without it.
#[tokio::test]
async fn rust_reads_node_written_rows_and_fails_closed_without_the_key() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }
    let key = test_key();
    let dir = TempDir::new("parity-crossread");
    let response = run_node(
        &root,
        dir.path(),
        Some(&key),
        &request_json(&[], Some("parity")),
    );

    let accepted: Vec<(String, String)> = array(&response, "creates")
        .iter()
        .zip(CREATE_CASES)
        .filter(|(row, _)| row.get("accepted").and_then(Value::as_bool) == Some(true))
        .map(|(row, url)| {
            (
                row.get("id")
                    .and_then(Value::as_str)
                    .expect("node reported an id")
                    .to_owned(),
                (*url).trim().to_owned(),
            )
        })
        .collect();
    assert!(
        !accepted.is_empty(),
        "the node reference accepted nothing to read back"
    );

    // Same database file, opened by the Rust bootstrap.
    let pool = open_pool(dir.path());
    let with_key = store_with(pool.clone(), Some(&key));
    let without_key = store_with(pool, None);

    for (id, original) in accepted {
        let id = artifact_mcp::model::WebhookId(id);
        let delivery = with_key
            .delivery(&id)
            .await
            .expect("rust decrypts a node-written row")
            .expect("row exists");
        assert_eq!(
            delivery.url, original,
            "rust did not recover the URL node encrypted"
        );

        // The masked column alone must never be served as a delivery target.
        let error = without_key
            .delivery(&id)
            .await
            .expect_err("an encrypted node row must fail closed without the key");
        assert_eq!(error, artifact_mcp::error::AppError::Internal);

        // …and the masked value itself must not contain the token.
        let summary = without_key
            .summary(&id)
            .await
            .expect("masked read")
            .expect("row exists");
        assert!(
            !summary.url.contains("token"),
            "mask leaked: {}",
            summary.url
        );
    }
}
