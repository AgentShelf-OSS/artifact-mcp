//! U07 cross-runtime proof: the bundle sanitizer, the canonical digests, the limit messages,
//! entry selection, and the MIME table must agree with the Node reference exactly.
//!
//! Digest and sanitizer bugs are invisible in Rust-only tests — a wrong-but-self-consistent
//! implementation passes every unit test and then silently orphans every preview file and every
//! crash-recovery comparison. So the oracle is the real `lib/store.js`.
//!
//! The harness drives `createArtifactStore` — the exported factory, not a copy of its internals
//! — with a stub database and a stub filesystem. `publishBundle` therefore runs the genuine
//! `sanitizeRel`, `validateBundle`, entry selection, limit checks, and `bundleManifestDigest`,
//! and the captured INSERT row carries the digest Node would have committed.
//!
//! # Skipping
//!
//! Missing `node` or `node_modules` normally degrades to a skip so `cargo test` works in a
//! Rust-only environment. Set `REQUIRE_NODE_REFERENCE=1` to turn that into a hard failure — the
//! contract flags a silently-skipping cross-runtime test as a known hazard.

use std::path::{Path, PathBuf};
use std::process::Command;

use artifact_mcp::artifacts::digest::bundle_manifest_digest;
use artifact_mcp::artifacts::read::{DEFAULT_CONTENT_TYPE, mime_for};
use artifact_mcp::artifacts::validation::{
    sanitize_relative_path, validate_bundle, validate_single_body,
};
use artifact_mcp::config::StorageLimits;
use serde_json::{Value, json};

use crate::u03_support::TempDataDir;
use crate::u07_paths::{TRAVERSAL_CASES, mime_probes};

/// Executed inside the repo root with `DATA_DIR` pointing at a throwaway directory.
const HARNESS: &str = r#"
import path from "node:path";
import { readFileSync } from "node:fs";

const cases = JSON.parse(readFileSync(process.argv[2], "utf8"));
const repoRoot = process.argv[3];
const { createArtifactStore } = await import(new URL("lib/store.js", `file://${repoRoot}/`).href);

const ARTIFACT_DIR = path.join(process.env.DATA_DIR, "artifacts");
const ID = "abc123def456";

function stubFiles(overrides) {
  return {
    cpSync() {},
    existsSync() { return false; },
    mkdirSync() {},
    readFileSync() { return Buffer.alloc(0); },
    readdirSync() { return []; },
    renameSync() {},
    rmSync() {},
    statSync() { return { isFile: () => true, isDirectory: () => false }; },
    writeFileSync() {},
    ...overrides
  };
}

function limitsOf(testCase) {
  const limits = {};
  for (const key of ["maxBytes", "maxBundleBytes", "maxBundleFiles"]) {
    if (testCase[key] !== undefined && testCase[key] !== null) limits[key] = testCase[key];
  }
  return limits;
}

function makeStore({ limits = {}, meta = undefined, files = {} } = {}) {
  const rows = [];
  const statement = {
    run(row) {
      if (row && typeof row === "object" && "body_sha256" in row) rows.push(row);
      return { changes: 1 };
    },
    get() { return meta ?? (rows[0] ? { ...rows[0], revision: 1 } : undefined); },
    all() { return []; }
  };
  const database = {
    prepare() { return statement; },
    transaction(fn) { return (...args) => fn(...args); },
    pragma() {}
  };
  const store = createArtifactStore({
    db: database,
    artifactDir: ARTIFACT_DIR,
    files: stubFiles(files),
    idFactory: () => ID,
    orgExists: () => true,
    ...limits
  });
  return { store, rows };
}

function runSingle(testCase) {
  const { store, rows } = makeStore({ limits: limitsOf(testCase) });
  try {
    const result = store.publish({
      clientId: "cid", org: "default", uploaderLabel: "u",
      html: testCase.html, title: "t", description: "d", category: ""
    });
    return { ok: true, bytes: result.bytes, digest: rows[rows.length - 1].body_sha256 };
  } catch (error) {
    return { ok: false, error: String(error && error.message) };
  }
}

function runBundle(testCase) {
  const { store, rows } = makeStore({ limits: limitsOf(testCase) });
  try {
    const files = {};
    for (const [name, content] of testCase.files) files[name] = content;
    const result = store.publishBundle({
      clientId: "cid", org: "default", uploaderLabel: "u", files,
      entry: testCase.entry === null ? undefined : testCase.entry,
      title: "t", description: "d", category: ""
    });
    return {
      ok: true, bytes: result.bytes, entry: result.entry, files: result.files,
      digest: rows[rows.length - 1].body_sha256
    };
  } catch (error) {
    return { ok: false, error: String(error && error.message) };
  }
}

function runMime(rel) {
  const { store } = makeStore({
    meta: { id: ID, is_bundle: 1, entry: "index.html" },
    files: { existsSync: () => true }
  });
  const result = store.readBundleFile(ID, rel);
  if (!result) return { kind: "null", value: null };
  const kind = typeof result.contentType;
  return { kind, value: kind === "string" ? result.contentType : null };
}

process.stdout.write(JSON.stringify({
  single: cases.single.map(runSingle),
  bundles: cases.bundles.map(runBundle),
  mime: cases.mime.map(runMime)
}));
"#;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Node reference availability. `REQUIRE_NODE_REFERENCE=1` converts a skip into a failure so CI
/// can assert the proof actually ran.
fn node_reference_available(root: &Path) -> bool {
    let required = std::env::var("REQUIRE_NODE_REFERENCE").is_ok_and(|value| value == "1");
    let missing = if root.join("node_modules/better-sqlite3").is_dir() {
        match Command::new("node").arg("--version").output() {
            Ok(output) if output.status.success() => None,
            _ => Some("node is not on PATH"),
        }
    } else {
        Some("node_modules/better-sqlite3 is missing")
    };
    match missing {
        None => true,
        Some(reason) => {
            assert!(
                !required,
                "REQUIRE_NODE_REFERENCE=1 but the Node reference is unusable: {reason}"
            );
            eprintln!("skipping U07 Node parity proof: {reason}");
            false
        }
    }
}

fn run_harness(cases: &Value) -> Value {
    let root = repo_root();
    let dir = TempDataDir::new("u07-parity");
    let harness = dir.path().join("u07-harness.mjs");
    let case_file = dir.path().join("u07-cases.json");
    std::fs::write(&harness, HARNESS).expect("write harness");
    std::fs::write(&case_file, cases.to_string()).expect("write cases");

    let output = Command::new("node")
        .current_dir(&root)
        .arg(&harness)
        .arg(&case_file)
        .arg(&root)
        .env("DATA_DIR", dir.path())
        .env_remove("WEBHOOK_ENC_KEY")
        .env_remove("ARTIFACT_API_KEYS")
        .env_remove("ORG_EMAIL_DOMAINS")
        .output()
        .expect("run node harness");
    assert!(
        output.status.success(),
        "node harness failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "node harness produced unparsable output ({error}):\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

// ---------------------------------------------------------------------------
// Case construction — one table, replayed by both runtimes
// ---------------------------------------------------------------------------

struct SingleCase {
    html: String,
    max_bytes: Option<u64>,
}

struct BundleCase {
    files: Vec<(String, String)>,
    entry: Option<String>,
    max_bundle_bytes: Option<u64>,
    max_bundle_files: Option<u64>,
}

fn single_cases() -> Vec<SingleCase> {
    let mut cases: Vec<SingleCase> = [
        "<h1>hi</h1>",
        "日本語",
        "<p>é 😀 \u{1d54f}</p>",
        "line\nbreak\ttab \"quote\" \\slash",
        "",
        "   ",
        "\t\n\r ",
        // U+FEFF is ECMAScript whitespace, so Node trims it away and rejects the body.
        "\u{feff}",
        // U+0085 is Unicode White_Space but NOT ECMAScript whitespace: Node accepts it.
        "\u{85}x",
        "\u{a0}\u{2028}\u{3000}",
    ]
    .into_iter()
    .map(|html| SingleCase {
        html: html.to_owned(),
        max_bytes: None,
    })
    .collect();

    cases.push(SingleCase {
        html: "12345678".to_owned(),
        max_bytes: Some(8),
    });
    cases.push(SingleCase {
        html: "123456789".to_owned(),
        max_bytes: Some(8),
    });
    cases.push(SingleCase {
        html: "日本語".to_owned(),
        max_bytes: Some(8),
    });
    cases
}

fn owned(files: &[(&str, &str)]) -> Vec<(String, String)> {
    files
        .iter()
        .map(|(name, content)| ((*name).to_owned(), (*content).to_owned()))
        .collect()
}

fn bundle_cases() -> Vec<BundleCase> {
    let mut cases: Vec<BundleCase> = Vec::new();

    // 1. Every adversarial path, alongside a valid entry so the discriminator is the path alone.
    for (raw, _) in TRAVERSAL_CASES {
        cases.push(BundleCase {
            files: vec![
                ("index.html".to_owned(), "<h1>hi</h1>".to_owned()),
                ((*raw).to_owned(), "payload".to_owned()),
            ],
            entry: None,
            max_bundle_bytes: None,
            max_bundle_files: None,
        });
    }

    /// A literal bundle spec: its files, and the entry the caller asked for.
    type PlainCase = (
        &'static [(&'static str, &'static str)],
        Option<&'static str>,
    );

    let plain: &[PlainCase] = &[
        // 2. Digest shape: escaping, non-ASCII, and astral sort keys.
        (
            &[
                ("index.html", "a"),
                ("\u{1d54f}/a.html", "b"),
                ("\u{e000}/b.html", "c"),
                ("he\"llo.txt", "d"),
                ("tab\tfile.txt", "e"),
                ("nl\nfile.txt", "f"),
                ("ünïcode/файл.html", "g"),
                ("back\\slash", "h"),
            ],
            None,
        ),
        // 3. Two raw names collapsing to the same relative path: both survive, bytes double count.
        (&[("index.html", "aa"), ("./index.html", "bbb")], None),
        // 4. Entry selection.
        (&[("z.html", "z"), ("a.html", "a")], None),
        (&[("z.html", "z"), ("index.html", "i")], None),
        (&[("page.htm", "p"), ("style.css", "c")], None),
        (&[("index.html", "i")], Some("missing.html")),
        (&[("index.html", "i")], Some("../evil")),
        (&[("index.html", "i")], Some("")),
        (&[("index.html", "i"), ("a.html", "a")], Some("./a.html")),
        (&[("page.htm", "p")], Some("page.htm")),
        (&[("style.css", "c")], None),
        (&[("deep/nested/index.html", "d")], None),
        (&[], None),
    ];
    for (files, entry) in plain {
        cases.push(BundleCase {
            files: owned(files),
            entry: (*entry).map(str::to_owned),
            max_bundle_bytes: None,
            max_bundle_files: None,
        });
    }

    // 5. Limit boundaries.
    cases.push(BundleCase {
        files: owned(&[("index.html", "12345"), ("a.css", "12345")]),
        entry: None,
        max_bundle_bytes: Some(10),
        max_bundle_files: None,
    });
    cases.push(BundleCase {
        files: owned(&[("index.html", "123456"), ("a.css", "12345")]),
        entry: None,
        max_bundle_bytes: Some(10),
        max_bundle_files: None,
    });
    cases.push(BundleCase {
        files: owned(&[("index.html", "1"), ("a.css", "2")]),
        entry: None,
        max_bundle_bytes: None,
        max_bundle_files: Some(2),
    });
    cases.push(BundleCase {
        files: owned(&[("index.html", "1"), ("a.css", "2"), ("b.css", "3")]),
        entry: None,
        max_bundle_bytes: None,
        max_bundle_files: Some(2),
    });
    // The count check must beat the path check.
    cases.push(BundleCase {
        files: owned(&[("../a", "1"), ("../b", "2"), ("../c", "3")]),
        entry: None,
        max_bundle_bytes: None,
        max_bundle_files: Some(2),
    });
    // …and the path check must beat the byte total.
    cases.push(BundleCase {
        files: owned(&[("index.html", "123456"), ("../x", "12345")]),
        entry: None,
        max_bundle_bytes: Some(10),
        max_bundle_files: None,
    });
    cases
}

fn limits_for(
    max_bytes: Option<u64>,
    bundle_bytes: Option<u64>,
    bundle_files: Option<u64>,
) -> StorageLimits {
    let defaults = StorageLimits::default();
    StorageLimits {
        max_artifact_bytes: max_bytes.unwrap_or(defaults.max_artifact_bytes),
        max_bundle_bytes: bundle_bytes.unwrap_or(defaults.max_bundle_bytes),
        max_bundle_files: bundle_files.unwrap_or(defaults.max_bundle_files),
        ..defaults
    }
}

fn rust_single(case: &SingleCase) -> Value {
    let limits = limits_for(case.max_bytes, None, None);
    match validate_single_body(&case.html, &limits) {
        Ok(body) => json!({ "ok": true, "bytes": body.bytes, "digest": body.body_sha256 }),
        Err(error) => json!({ "ok": false, "error": error.to_string() }),
    }
}

fn rust_bundle(case: &BundleCase) -> Value {
    let limits = limits_for(None, case.max_bundle_bytes, case.max_bundle_files);
    match validate_bundle(&case.files, case.entry.as_deref(), None, &limits) {
        Ok(bundle) => json!({
            "ok": true,
            "bytes": bundle.total_bytes,
            "entry": bundle.entry,
            "files": bundle.files.len(),
            "digest": bundle_manifest_digest(&bundle.files),
        }),
        Err(error) => json!({ "ok": false, "error": error.to_string() }),
    }
}

fn rust_mime(rel: &str) -> Value {
    // Mirrors `readBundleFile`: reject first, then map the sanitized relative path.
    sanitize_relative_path(rel).map_or_else(
        || json!({ "kind": "null", "value": null }),
        |sanitized| json!({ "kind": "string", "value": mime_for(&sanitized) }),
    )
}

// ---------------------------------------------------------------------------
// The proof
// ---------------------------------------------------------------------------

#[test]
fn rust_and_node_agree_on_sanitizing_digests_limits_and_entries() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let singles = single_cases();
    let bundles = bundle_cases();
    let mimes = mime_probes();

    let cases = json!({
        "single": singles
            .iter()
            .map(|case| json!({ "html": case.html, "maxBytes": case.max_bytes }))
            .collect::<Vec<_>>(),
        "bundles": bundles
            .iter()
            .map(|case| json!({
                "files": case.files
                    .iter()
                    .map(|(name, content)| json!([name, content]))
                    .collect::<Vec<_>>(),
                "entry": case.entry,
                "maxBundleBytes": case.max_bundle_bytes,
                "maxBundleFiles": case.max_bundle_files,
            }))
            .collect::<Vec<_>>(),
        "mime": mimes.clone(),
    });

    let node = run_harness(&cases);

    let node_single = node["single"].as_array().expect("node single results");
    assert_eq!(node_single.len(), singles.len());
    for (case, expected) in singles.iter().zip(node_single) {
        assert_eq!(
            &rust_single(case),
            expected,
            "single-file parity diverged for html {:?} (maxBytes {:?})",
            case.html,
            case.max_bytes
        );
        assert!(
            expected["ok"].as_bool() == Some(false) || expected["digest"].is_string(),
            "node produced no digest for {:?}",
            case.html
        );
    }

    let node_bundles = node["bundles"].as_array().expect("node bundle results");
    assert_eq!(node_bundles.len(), bundles.len());
    for (case, expected) in bundles.iter().zip(node_bundles) {
        assert_eq!(
            &rust_bundle(case),
            expected,
            "bundle parity diverged for {:?} (entry {:?})",
            case.files.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            case.entry
        );
    }

    let node_mime = node["mime"].as_array().expect("node mime results");
    assert_eq!(node_mime.len(), mimes.len());
    for (rel, expected) in mimes.iter().zip(node_mime) {
        assert_eq!(
            &rust_mime(rel),
            expected,
            "MIME parity diverged for {rel:?}"
        );
    }

    // The proof is only worth something if it actually exercised both outcomes.
    assert!(
        node_bundles
            .iter()
            .any(|result| result["ok"].as_bool() == Some(true))
            && node_bundles
                .iter()
                .any(|result| result["ok"].as_bool() == Some(false)),
        "the bundle table stopped covering both accept and reject"
    );
}

/// Documents the one place Rust deliberately does not reproduce Node: `mimeFor` indexes a plain
/// object literal, so a bundle file whose last dot-segment names an `Object.prototype` member
/// resolves to an inherited value instead of `undefined`, and the fallback `||` never fires.
/// Node then hands Express a non-string content type; Rust returns `application/octet-stream`.
///
/// `mimeFor` lowercases first, so only members that are already lowercase can leak —
/// `constructor` and `__proto__`. `toString`/`valueOf` become `tostring`/`valueof` and land on
/// the normal fallback, which this test also pins.
#[test]
fn node_mime_lookup_leaks_object_prototype_members() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let probes = [
        ("x.constructor", "function"),
        ("x.__proto__", "object"),
        ("x.toString", "string"),
        ("x.valueOf", "string"),
    ];
    let cases = json!({
        "single": Vec::<Value>::new(),
        "bundles": Vec::<Value>::new(),
        "mime": probes.iter().map(|(probe, _)| *probe).collect::<Vec<_>>(),
    });
    let node = run_harness(&cases);

    for ((probe, kind), result) in probes
        .iter()
        .zip(node["mime"].as_array().expect("mime results"))
    {
        assert_eq!(
            result["kind"].as_str(),
            Some(*kind),
            "Node's `mimeFor` lookup changed for {probe:?} — revisit the divergence note in \
             read.rs"
        );
        // Rust has no prototype chain, so every one of these is the plain fallback.
        assert_eq!(mime_for(probe), DEFAULT_CONTENT_TYPE, "{probe}");
        if *kind == "string" {
            assert_eq!(result["value"].as_str(), Some(DEFAULT_CONTENT_TYPE));
        }
    }
}
