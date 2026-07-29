//! U16 cross-runtime proof: PNG validation, the placeholder bytes, and the digest-addressed
//! path must agree with `lib/thumbnails.js`.
//!
//! Three of this unit's outputs are shared with the Node reference and cannot be proved by a
//! Rust-only round trip:
//!
//! * **the path.** `/data/previews/<id>/<body_sha256>.png` is a live directory both runtimes read
//!   during cutover. The strongest available proof is behavioural: Rust writes a thumbnail at the
//!   path *it* computes, and Node's own `readThumbnail` — with its own `thumbnailPath` — has to
//!   find it. A one-character divergence makes Node return `null`.
//! * **the placeholder SVG**, which is served byte-for-byte to browsers today, including the
//!   `safeColor` hue derived from a 32-bit FNV-1a over UTF-16 code units.
//! * **the validators.** `validPng`, `validArtifactId` and `validDigest` are the guards that keep
//!   caller-controlled strings out of filesystem paths; a laxer Rust regex would be a hole.
//!
//! # Skip visibility
//!
//! These tests **skip** when `node` or `lib/thumbnails.js` is unavailable so `cargo test` still
//! works in a Rust-only environment. Per the U01 contract (§"RESOLVED at M2 — cross-runtime skip
//! hazard") that skip is a hazard, so `REQUIRE_NODE_REFERENCE=1` converts it into a hard failure:
//!
//! ```text
//! REQUIRE_NODE_REFERENCE=1 cargo test
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use artifact_mcp::artifacts::paths::{BodyDigest, SafeArtifactId, thumbnail_path};
use artifact_mcp::integrations::thumbnails::{
    DEFAULT_MAX_PNG_BYTES, PNG_SIGNATURE, thumbnail_placeholder, valid_png,
};
use artifact_mcp::model::{ArtifactId, ArtifactMeta, ClientId, OrgId, Timestamp};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};

use crate::u03_support::TempDataDir;
use crate::u16_support::png_of;

/// Setting this to `1` turns "Node is unavailable" from a skip into a failure.
const REQUIRE_NODE_REFERENCE: &str = "REQUIRE_NODE_REFERENCE";

const ID: &str = "abc123def456";
const DIGEST: &str = "cafebabe00112233445566778899aabbccddeeff00112233445566778899aabb";
const OTHER_DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// One `node -e` invocation covering every `lib/thumbnails.js` entry point this unit ports.
const NODE_DRIVER: &str = r#"
import(process.argv[1]).then(async (thumbnails) => {
  const input = JSON.parse(process.argv[2]);
  const store = thumbnails.createThumbnailStore({ dataDir: input.dataDir });
  const out = {
    defaultMaxPngBytes: thumbnails.DEFAULT_MAX_PNG_BYTES,
    signature: [...thumbnails.PNG_SIGNATURE],
    validPng: input.pngCases.map((probe) =>
      thumbnails.validPng(Buffer.from(probe.bytes, "base64"), probe.maxBytes)),
    validArtifactId: input.ids.map((value) => thumbnails.validArtifactId(value)),
    validDigest: input.digests.map((value) => thumbnails.validDigest(value)),
    placeholders: input.placeholders.map((probe) =>
      thumbnails.thumbnailPlaceholder(probe.meta, probe.accent).toString("base64")),
    reads: []
  };
  for (const probe of input.reads) {
    const png = await store.readThumbnail(probe.meta, probe.requested);
    out.reads.push(png ? png.toString("base64") : null);
  }
  process.stdout.write(JSON.stringify(out));
}).catch((error) => { console.error(error); process.exit(1); });
"#;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn require_node_reference() -> bool {
    std::env::var(REQUIRE_NODE_REFERENCE).is_ok_and(|value| value == "1")
}

/// Node reference availability.
///
/// Returns `false` (skip) only when `REQUIRE_NODE_REFERENCE=1` is not set; otherwise it fails the
/// test, so a CI job cannot silently green-pass without ever running the parity proof.
fn node_reference_available(root: &Path) -> bool {
    let unavailable = if root.join("lib/thumbnails.js").is_file() {
        match Command::new("node").arg("--version").output() {
            Ok(output) if output.status.success() => None,
            _ => Some("node is not on PATH"),
        }
    } else {
        Some("lib/thumbnails.js is missing")
    };

    match unavailable {
        None => true,
        Some(reason) => {
            assert!(
                !require_node_reference(),
                "{REQUIRE_NODE_REFERENCE}=1 but the Node reference is unavailable ({reason}); \
                 the thumbnail parity proof did not run"
            );
            eprintln!("skipping U16 Node parity proof: {reason}");
            eprintln!("set {REQUIRE_NODE_REFERENCE}=1 to make this a failure instead");
            false
        }
    }
}

fn run_node(root: &Path, request: &Value) -> Value {
    let module = format!("file://{}", root.join("lib/thumbnails.js").display());
    let output = Command::new("node")
        .current_dir(root)
        .arg("-e")
        .arg(NODE_DRIVER)
        .arg(&module)
        .arg(request.to_string())
        .env_remove("DATA_DIR")
        .env_remove("PREVIEW_MAX_PNG_BYTES")
        .env_remove("PREVIEW_RENDERER_URL")
        .output()
        .expect("run the node thumbnail reference");
    assert!(
        output.status.success(),
        "node reference failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("node reference emitted JSON")
}

fn meta_json(id: &str, digest: &str, is_bundle: bool, org: &str) -> Value {
    json!({ "id": id, "body_sha256": digest, "is_bundle": is_bundle, "org": org })
}

fn meta(id: &str, digest: &str, is_bundle: bool, org: &str) -> ArtifactMeta {
    ArtifactMeta {
        id: ArtifactId(id.to_owned()),
        client_id: ClientId("client".to_owned()),
        org: OrgId(org.to_owned()),
        title: String::new(),
        description: String::new(),
        bytes: 0,
        created_at: Timestamp(String::new()),
        updated_at: Timestamp(String::new()),
        uploader_label: String::new(),
        owner_email: None,
        is_bundle,
        entry: String::new(),
        revision: 1,
        category: String::new(),
        hidden: false,
        body_sha256: digest.to_owned(),
    }
}

/// PNG buffers and caps whose verdicts must match: valid, empty, one byte short of the
/// signature, a flipped signature byte, a JSON error page, exactly at the cap, and one over.
fn png_cases() -> Vec<(Vec<u8>, u64)> {
    let mut flipped = png_of(32);
    flipped[7] = 0x0b;
    vec![
        (png_of(64), DEFAULT_MAX_PNG_BYTES),
        (Vec::new(), DEFAULT_MAX_PNG_BYTES),
        (PNG_SIGNATURE.to_vec(), DEFAULT_MAX_PNG_BYTES),
        (PNG_SIGNATURE[..7].to_vec(), DEFAULT_MAX_PNG_BYTES),
        (flipped, DEFAULT_MAX_PNG_BYTES),
        (
            br#"{"error":"render failed"}"#.to_vec(),
            DEFAULT_MAX_PNG_BYTES,
        ),
        (png_of(1_024), 1_024),
        (png_of(1_025), 1_024),
        (png_of(64), 8),
        (b"PNG\x89\r\n\x1a\n".to_vec(), DEFAULT_MAX_PNG_BYTES),
    ]
}

/// Id shapes that must be accepted or rejected identically — the guard that keeps a caller
/// string out of a path segment.
fn id_cases() -> Vec<&'static str> {
    vec![
        ID,
        "abcdef",
        "abcde",
        "a".repeat(24).leak(),
        "a".repeat(25).leak(),
        "",
        "ABC123DEF456",
        "abc-123-def",
        "../../etc/passwd",
        "abc123/def456",
        "abc 123 def",
        ".hidden123",
        "thumbnails",
        "notifications",
        "abc123def456\n",
    ]
}

fn digest_cases() -> Vec<&'static str> {
    vec![
        DIGEST,
        OTHER_DIGEST,
        "",
        "CAFEBABE00112233445566778899AABBCCDDEEFF00112233445566778899AABB",
        &DIGEST[..63],
        "g".repeat(64).leak(),
        "../../../etc/passwd",
        "0",
    ]
}

/// Placeholder inputs: single vs bundle, explicit accents in both hex forms, unusable accents,
/// and org names that exercise the UTF-16 hash (ASCII, empty, emoji, astral pair).
fn placeholder_cases() -> Vec<(bool, &'static str, Option<&'static str>)> {
    vec![
        (false, "acme", None),
        (true, "acme", None),
        (false, "acme", Some("#123456")),
        (false, "acme", Some("#0a0")),
        (false, "acme", Some("  #ABCDEF  ")),
        (false, "acme", Some("red")),
        (false, "acme", Some("#12345")),
        (false, "acme", Some("")),
        (false, "", None),
        (false, "globex-int'l", None),
        (false, "ünïcødé-🎉-Ω", None),
        (false, "𝄞clef", None),
    ]
}

#[test]
fn node_agrees_on_validation_placeholders_and_the_digest_addressed_path() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    // A directory Rust populates using *its* path builder, which Node then has to read back.
    let data = TempDataDir::new("u16-parity");
    let png = png_of(96);
    let target = thumbnail_path(
        data.path(),
        &SafeArtifactId::parse(ID).expect("valid id"),
        &BodyDigest::parse(DIGEST).expect("valid digest"),
    );
    std::fs::create_dir_all(target.parent().expect("preview parent")).expect("preview dir");
    std::fs::write(&target, &png).expect("write thumbnail");

    let pngs = png_cases();
    let ids = id_cases();
    let digests = digest_cases();
    let placeholders = placeholder_cases();

    let request = json!({
        "dataDir": data.path().to_string_lossy(),
        "pngCases": pngs
            .iter()
            .map(|(bytes, max)| json!({ "bytes": BASE64.encode(bytes), "maxBytes": max }))
            .collect::<Vec<Value>>(),
        "ids": ids,
        "digests": digests,
        "placeholders": placeholders
            .iter()
            .map(|(is_bundle, org, accent)| json!({
                "meta": meta_json(ID, DIGEST, *is_bundle, org),
                "accent": accent,
            }))
            .collect::<Vec<Value>>(),
        "reads": [
            // The current digest: Node must find exactly the file Rust wrote.
            { "meta": meta_json(ID, DIGEST, false, "acme"), "requested": DIGEST },
            // A stale digest: both runtimes must refuse rather than serve an older render.
            { "meta": meta_json(ID, DIGEST, false, "acme"), "requested": OTHER_DIGEST },
            // A bundle never has a thumbnail, even with the file present.
            { "meta": meta_json(ID, DIGEST, true, "acme"), "requested": DIGEST },
            // An id that cannot form a path.
            { "meta": meta_json("../../etc", DIGEST, false, "acme"), "requested": DIGEST },
        ],
    });

    let node = run_node(&root, &request);

    // --- frozen constants -------------------------------------------------
    assert_eq!(
        node["defaultMaxPngBytes"].as_u64(),
        Some(DEFAULT_MAX_PNG_BYTES),
        "DEFAULT_MAX_PNG_BYTES diverged"
    );
    let signature: Vec<u64> = node["signature"]
        .as_array()
        .expect("signature array")
        .iter()
        .map(|byte| byte.as_u64().expect("signature byte"))
        .collect();
    assert_eq!(
        signature,
        PNG_SIGNATURE
            .iter()
            .map(|byte| u64::from(*byte))
            .collect::<Vec<u64>>(),
        "PNG_SIGNATURE diverged"
    );

    // --- validPng ---------------------------------------------------------
    let node_png = node["validPng"].as_array().expect("validPng array");
    assert_eq!(node_png.len(), pngs.len());
    for (index, (bytes, max)) in pngs.iter().enumerate() {
        assert_eq!(
            valid_png(bytes, *max),
            node_png[index].as_bool().expect("boolean verdict"),
            "validPng disagreed for case {index} ({} bytes, cap {max})",
            bytes.len()
        );
    }

    // --- id and digest guards --------------------------------------------
    let node_ids = node["validArtifactId"].as_array().expect("id array");
    for (index, id) in ids.iter().enumerate() {
        assert_eq!(
            SafeArtifactId::parse(id).is_some(),
            node_ids[index].as_bool().expect("boolean verdict"),
            "validArtifactId disagreed for {id:?}"
        );
    }
    let node_digests = node["validDigest"].as_array().expect("digest array");
    for (index, digest) in digests.iter().enumerate() {
        assert_eq!(
            BodyDigest::parse(digest).is_some(),
            node_digests[index].as_bool().expect("boolean verdict"),
            "validDigest disagreed for {digest:?}"
        );
    }

    // --- placeholder bytes ------------------------------------------------
    let node_placeholders = node["placeholders"].as_array().expect("placeholder array");
    for (index, (is_bundle, org, accent)) in placeholder_cases().into_iter().enumerate() {
        let expected = BASE64
            .decode(
                node_placeholders[index]
                    .as_str()
                    .expect("base64 placeholder"),
            )
            .expect("decode placeholder");
        let actual = thumbnail_placeholder(&meta(ID, DIGEST, is_bundle, org), accent);
        assert_eq!(
            String::from_utf8_lossy(&actual),
            String::from_utf8_lossy(&expected),
            "placeholder diverged for bundle={is_bundle} org={org:?} accent={accent:?}"
        );
    }

    // --- the digest-addressed path, proved through Node's own reader ------
    let reads = node["reads"].as_array().expect("reads array");
    assert_eq!(
        reads[0]
            .as_str()
            .map(|value| BASE64.decode(value).expect("decode png")),
        Some(png),
        "Node could not read the thumbnail Rust wrote: the preview path diverged"
    );
    assert!(reads[1].is_null(), "Node served a stale digest");
    assert!(reads[2].is_null(), "Node served a bundle thumbnail");
    assert!(reads[3].is_null(), "Node accepted an unvalidated id");
}
