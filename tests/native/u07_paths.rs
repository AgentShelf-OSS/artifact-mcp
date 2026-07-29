//! U07 native tests: the frozen path layout, the bundle path sanitizer, the MIME table, the
//! limit boundaries, entry selection, id validation, and the on-disk read APIs.
//!
//! The hostile-path table lives here and is *shared* with `u07_node_parity`, which replays the
//! exact same strings through the Node reference. A case added here is automatically proved
//! against the oracle.

use std::fs;
use std::path::Path;

use artifact_mcp::artifacts::digest::{body_digest_at_path, bundle_manifest_digest, sha256_hex};
use artifact_mcp::artifacts::paths::{
    self, BodyDigest, SafeArtifactId, TransientKind, is_transient_name, transient_name_artifact_id,
};
use artifact_mcp::artifacts::read::{
    DEFAULT_CONTENT_TYPE, MIME_TABLE, SINGLE_BODY_CONTENT_TYPE, mime_for, read_body,
    read_bundle_file, read_revision_body, read_revision_bundle_file, read_tree,
};
use artifact_mcp::artifacts::validation::{
    is_reserved_artifact_id, is_valid_digest, sanitize_relative_path, select_entry,
    validate_bundle, validate_single_body,
};
use artifact_mcp::config::StorageLimits;

use crate::u03_support::TempDataDir;

/// Adversarial relative paths. Every entry is replayed against Node in `u07_node_parity`, so
/// the expectations below are claims about the *reference*, not about a Rust-only opinion.
///
/// `None` = the sanitizer must reject; `Some(rel)` = it must normalise to exactly `rel`.
pub const TRAVERSAL_CASES: &[(&str, Option<&str>)] = &[
    // --- plain traversal ---------------------------------------------------
    ("../secret", None),
    ("../../etc/passwd", None),
    ("..", None),
    ("../", None),
    ("a/../../b", None),
    ("a/./../../b", None),
    ("a/b/../../../c", None),
    ("nested/deep/../../../escape", None),
    (".hidden/../../x", None),
    ("/../x", None),
    ("/./../x", None),
    ("./../x", None),
    // --- separator laundering ----------------------------------------------
    ("..\\x", None),
    ("..\\..\\etc\\passwd", None),
    ("..\\/..\\/x", None),
    ("x/\\../..", None),
    ("\\", None),
    ("\\..", None),
    // --- degenerate / empty -------------------------------------------------
    ("", None),
    (".", None),
    ("/", None),
    ("//", None),
    ("///", None),
    // --- accepted, and exactly how they normalise ---------------------------
    // A leading slash is stripped, not rejected: the result is a literal relative name.
    ("/etc/passwd", Some("etc/passwd")),
    ("//etc/passwd", Some("etc/passwd")),
    // UNC-looking input collapses to a plain relative name after the strip.
    ("\\\\server\\share\\x", Some("server/share/x")),
    // Drive-qualified input survives as a literal directory called `C:` (delta request 2).
    ("C:\\Windows\\system32", Some("C:/Windows/system32")),
    ("C:/x", Some("C:/x")),
    // Percent-encoding is never decoded, so encoded traversal stays one literal filename.
    ("..%2f..%2fetc", Some("..%2f..%2fetc")),
    ("%2e%2e/%2e%2e/etc", Some("%2e%2e/%2e%2e/etc")),
    ("..%5c..%5cetc", Some("..%5c..%5cetc")),
    // Three or more dots are an ordinary segment name.
    ("...", Some("...")),
    ("....//x", Some("..../x")),
    ("..a", Some("..a")),
    ("a..", Some("a..")),
    ("a/..b/c", Some("a/..b/c")),
    // Interior `..` that stays inside the root is resolved, not rejected.
    ("a/../b", Some("b")),
    ("a/b/./../c", Some("a/c")),
    ("./a", Some("a")),
    ("a//b", Some("a/b")),
    // Node keeps a trailing separator, and accepts a bare `./`.
    ("a/", Some("a/")),
    ("./", Some("./")),
    ("a/b/", Some("a/b/")),
    // Non-ASCII and astral names are ordinary names.
    ("ünïcode/файл.html", Some("ünïcode/файл.html")),
    ("\u{1d54f}/a.html", Some("\u{1d54f}/a.html")),
    ("\u{e000}/b.html", Some("\u{e000}/b.html")),
    // Characters that must survive JSON escaping in the manifest digest.
    ("he\"llo.txt", Some("he\"llo.txt")),
    ("back\\slash", Some("back/slash")),
    ("tab\tfile.txt", Some("tab\tfile.txt")),
    ("nl\nfile.txt", Some("nl\nfile.txt")),
    ("   ", Some("   ")),
];

/// MIME probes replayed against Node. Covers the whole frozen table plus the `split(".").pop()`
/// edge cases.
pub fn mime_probes() -> Vec<String> {
    let mut probes: Vec<String> = Vec::new();
    for (extension, _) in MIME_TABLE {
        probes.push(format!("file.{extension}"));
        probes.push(format!("dir/FILE.{}", extension.to_uppercase()));
    }
    probes.extend(
        [
            "noextension",
            "archive.tar.gz",
            "trailing.",
            ".hidden",
            "a.b.c.html",
            "weird.HtMl",
            "x.unknownext",
        ]
        .into_iter()
        .map(str::to_owned),
    );
    probes
}

fn id() -> SafeArtifactId {
    SafeArtifactId::parse("abc123def456").expect("valid id")
}

fn limits(artifact: u64, bundle_bytes: u64, bundle_files: u64) -> StorageLimits {
    StorageLimits {
        max_artifact_bytes: artifact,
        max_bundle_bytes: bundle_bytes,
        max_bundle_files: bundle_files,
        ..StorageLimits::default()
    }
}

fn bundle(files: &[(&str, &str)]) -> Vec<(String, String)> {
    files
        .iter()
        .map(|(name, content)| ((*name).to_owned(), (*content).to_owned()))
        .collect()
}

// ---------------------------------------------------------------------------
// Sanitizer
// ---------------------------------------------------------------------------

#[test]
fn sanitizer_decides_every_adversarial_path_the_way_node_does() {
    for (raw, expected) in TRAVERSAL_CASES {
        assert_eq!(
            sanitize_relative_path(raw).as_deref(),
            *expected,
            "sanitizeRel({raw:?})"
        );
    }
}

#[test]
fn every_accepted_path_stays_inside_the_bundle_root() {
    let root = Path::new("/data/artifacts/abc123def456");
    for (raw, expected) in TRAVERSAL_CASES {
        let Some(rel) = expected else { continue };
        let full = artifact_mcp::artifacts::validation::contained_path(root, rel)
            .unwrap_or_else(|| panic!("containment rejected an accepted path: {raw:?}"));
        assert!(
            full.starts_with(root),
            "{raw:?} resolved outside the root: {full:?}"
        );
    }
}

#[test]
fn containment_rejects_a_sanitizer_bypass() {
    // If the sanitizer ever regressed, this second gate still refuses to leave the root.
    let root = Path::new("/data/artifacts/abc123def456");
    for rel in ["../elsewhere", "../../etc/passwd", "a/../../../etc"] {
        assert!(
            artifact_mcp::artifacts::validation::contained_path(root, rel).is_none(),
            "containment accepted {rel:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

#[test]
fn artifact_id_validation_matches_the_node_regex_and_reserved_set() {
    for reserved in [
        "mcp",
        "health",
        "settings",
        "raw",
        "s",
        "favicon.ico",
        "robots.txt",
        "",
    ] {
        assert!(is_reserved_artifact_id(reserved), "{reserved:?}");
        assert!(SafeArtifactId::addressable(reserved).is_none());
    }
    for bad in [
        "abcde",                     // 5 characters, below the minimum
        "abcdefghijklmnopqrstuvwxy", // 25 characters, above the maximum
        "ABCDEF",
        "abc-def",
        "abc.def",
        "abc/def",
        "../../etc",
        "abc def",
        "ábcdef",
    ] {
        assert!(is_reserved_artifact_id(bad), "{bad:?} should be unusable");
        assert!(SafeArtifactId::parse(bad).is_none(), "{bad:?}");
    }
    for good in ["abcdef", "abc123def456", "000000", "z".repeat(24).as_str()] {
        assert!(!is_reserved_artifact_id(good), "{good:?}");
        assert!(SafeArtifactId::parse(good).is_some());
    }

    // Contract-delta request 1: real top-level routes that Node's RESERVED set omits.
    for shadowing in ["thumbnails", "notifications"] {
        assert!(
            !is_reserved_artifact_id(shadowing),
            "Node's RESERVED set has been changed; update the U07 delta request"
        );
    }
}

#[test]
fn digest_validation_matches_the_node_regex() {
    let valid = sha256_hex(b"body");
    assert!(is_valid_digest(&valid));
    assert!(BodyDigest::parse(&valid).is_some());
    for bad in [
        "",
        "abc",
        &valid.to_uppercase(),
        &format!("{valid}0"),
        &valid.replace('a', "g"),
        "../../../etc/passwd",
    ] {
        assert!(!is_valid_digest(bad), "{bad:?}");
        assert!(BodyDigest::parse(bad).is_none());
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

#[test]
fn paths_match_the_frozen_blueprint_layout() {
    let data = Path::new("/data");
    let artifacts = paths::artifact_dir(data);
    let id = id();
    let digest = BodyDigest::parse(&sha256_hex(b"body")).expect("digest");

    assert_eq!(artifacts, Path::new("/data/artifacts"));
    assert_eq!(
        paths::single_body_path(&artifacts, &id),
        Path::new("/data/artifacts/abc123def456.html")
    );
    assert_eq!(
        paths::bundle_dir(&artifacts, &id),
        Path::new("/data/artifacts/abc123def456")
    );
    assert_eq!(
        paths::body_path(&artifacts, &id, false),
        paths::single_body_path(&artifacts, &id)
    );
    assert_eq!(
        paths::body_path(&artifacts, &id, true),
        paths::bundle_dir(&artifacts, &id)
    );
    assert_eq!(
        paths::history_root(&artifacts),
        Path::new("/data/artifacts/.history")
    );
    assert_eq!(
        paths::history_dir(&artifacts, &id),
        Path::new("/data/artifacts/.history/abc123def456")
    );
    assert_eq!(
        paths::history_body_path(&artifacts, &id, 3, false),
        Path::new("/data/artifacts/.history/abc123def456/3.html")
    );
    assert_eq!(
        paths::history_body_path(&artifacts, &id, 3, true),
        Path::new("/data/artifacts/.history/abc123def456/3")
    );
    assert_eq!(
        paths::transient_path(&artifacts, &id, TransientKind::Staging, "0123456789ab"),
        Some("/data/artifacts/.abc123def456.staging-0123456789ab".into())
    );
    assert_eq!(
        paths::transient_path(&artifacts, &id, TransientKind::Trash, "0123456789ab"),
        Some("/data/artifacts/.abc123def456.trash-0123456789ab".into())
    );
    assert_eq!(
        paths::thumbnail_path(data, &id, &digest),
        Path::new("/data/previews/abc123def456").join(format!("{}.png", digest.as_str()))
    );
}

#[test]
fn transient_names_round_trip_and_reject_junk() {
    let name = ".abc123def456.staging-0123456789ab";
    assert!(is_transient_name(name));
    assert_eq!(
        transient_name_artifact_id(name).map(|id| id.as_str().to_owned()),
        Some("abc123def456".to_owned())
    );
    assert_eq!(
        transient_name_artifact_id(".abc123def456.trash-0123456789ab")
            .map(|id| id.as_str().to_owned()),
        Some("abc123def456".to_owned())
    );
    for junk in [
        "abc123def456.staging-x",  // no leading dot
        ".ABC123DEF456.staging-x", // uppercase id
        ".abc.staging-x",          // id too short
        ".abc123def456.other-x",   // unknown kind
        ".abc123def456.staging",   // missing separator
        ".history",
    ] {
        assert!(transient_name_artifact_id(junk).is_none(), "{junk}");
    }
    assert!(!is_transient_name(".history"));
}

#[test]
fn path_construction_refuses_an_unvalidated_id() {
    assert!(SafeArtifactId::parse("../../etc").is_none());
    assert!(SafeArtifactId::parse(".history").is_none());
    assert!(
        paths::transient_path(
            Path::new("/data/artifacts"),
            &id(),
            TransientKind::Staging,
            "../escape"
        )
        .is_none()
    );
}

// ---------------------------------------------------------------------------
// MIME
// ---------------------------------------------------------------------------

#[test]
fn mime_table_covers_every_frozen_extension() {
    let expected: &[(&str, &str)] = &[
        ("index.html", "text/html; charset=utf-8"),
        ("index.htm", "text/html; charset=utf-8"),
        ("a/style.css", "text/css; charset=utf-8"),
        ("app.js", "text/javascript; charset=utf-8"),
        ("app.mjs", "text/javascript; charset=utf-8"),
        ("data.json", "application/json"),
        ("logo.svg", "image/svg+xml"),
        ("shot.png", "image/png"),
        ("photo.jpg", "image/jpeg"),
        ("photo.jpeg", "image/jpeg"),
        ("anim.gif", "image/gif"),
        ("pic.webp", "image/webp"),
        ("favicon.ico", "image/x-icon"),
        ("font.woff2", "font/woff2"),
        ("font.woff", "font/woff"),
        ("font.ttf", "font/ttf"),
        ("notes.txt", "text/plain; charset=utf-8"),
        ("app.js.map", "application/json"),
        ("feed.xml", "application/xml"),
    ];
    assert_eq!(expected.len(), MIME_TABLE.len());
    for (rel, mime) in expected {
        assert_eq!(mime_for(rel), *mime, "{rel}");
        assert_eq!(mime_for(&rel.to_uppercase()), *mime, "uppercase {rel}");
    }
    for rel in [
        "noextension",
        "archive.tar.gz",
        "trailing.",
        "",
        ".hidden",
        "x.unknownext",
    ] {
        assert_eq!(mime_for(rel), DEFAULT_CONTENT_TYPE, "{rel}");
    }
    assert_eq!(SINGLE_BODY_CONTENT_TYPE, "text/html; charset=utf-8");
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

#[test]
fn single_body_limits_are_enforced_at_the_boundary() {
    let limits = limits(8, 1024, 10);
    assert_eq!(
        validate_single_body("12345678", &limits)
            .expect("exactly the limit is accepted")
            .bytes,
        8
    );
    assert_eq!(
        validate_single_body("123456789", &limits)
            .unwrap_err()
            .to_string(),
        "html exceeds 8 bytes (got 9)"
    );
    // Byte length, not character length: three 3-byte characters overflow an 8-byte limit.
    assert_eq!(
        validate_single_body("日本語", &limits)
            .unwrap_err()
            .to_string(),
        "html exceeds 8 bytes (got 9)"
    );
    for blank in ["", " ", "\t\n\r ", "\u{feff}", "\u{a0}\u{2028}"] {
        assert_eq!(
            validate_single_body(blank, &limits)
                .unwrap_err()
                .to_string(),
            "html is required",
            "{blank:?}"
        );
    }
    // The blank check runs before the size check.
    let oversized_blank = " ".repeat(100);
    assert_eq!(
        validate_single_body(&oversized_blank, &limits)
            .unwrap_err()
            .to_string(),
        "html is required"
    );
}

#[test]
fn bundle_limits_are_enforced_at_the_boundary() {
    let limits = limits(1024, 10, 2);
    assert!(validate_bundle(&bundle(&[]), None, None, &limits).is_err());
    assert_eq!(
        validate_bundle(&bundle(&[]), None, None, &limits)
            .unwrap_err()
            .to_string(),
        "files is empty"
    );

    let two = bundle(&[("index.html", "12345"), ("a.css", "12345")]);
    assert_eq!(
        validate_bundle(&two, None, None, &limits)
            .expect("exactly the byte limit is accepted")
            .total_bytes,
        10
    );

    let three = bundle(&[("index.html", "1"), ("a.css", "2"), ("b.css", "3")]);
    assert_eq!(
        validate_bundle(&three, None, None, &limits)
            .unwrap_err()
            .to_string(),
        "too many files (max 2)"
    );

    let heavy = bundle(&[("index.html", "123456"), ("a.css", "12345")]);
    assert_eq!(
        validate_bundle(&heavy, None, None, &limits)
            .unwrap_err()
            .to_string(),
        "bundle exceeds 10 bytes (got 11)"
    );

    // The file-count check runs before path sanitizing.
    let many_hostile = bundle(&[("../a", "1"), ("../b", "1"), ("../c", "1")]);
    assert_eq!(
        validate_bundle(&many_hostile, None, None, &limits)
            .unwrap_err()
            .to_string(),
        "too many files (max 2)"
    );
    // …and path sanitizing runs before the byte total.
    let hostile_and_heavy = bundle(&[("index.html", "123456"), ("../x", "12345")]);
    assert_eq!(
        validate_bundle(&hostile_and_heavy, None, None, &limits)
            .unwrap_err()
            .to_string(),
        "unsafe file path: ../x"
    );
}

#[test]
fn every_rejected_path_fails_the_bundle_with_nodes_message() {
    let limits = StorageLimits::default();
    for (raw, expected) in TRAVERSAL_CASES {
        if expected.is_some() {
            continue;
        }
        let files = bundle(&[("index.html", "<h1>hi</h1>")])
            .into_iter()
            .chain(std::iter::once(((*raw).to_owned(), "x".to_owned())))
            .collect::<Vec<_>>();
        assert_eq!(
            validate_bundle(&files, None, None, &limits)
                .unwrap_err()
                .to_string(),
            format!("unsafe file path: {raw}"),
            "{raw:?} should have been rejected"
        );
    }
}

// ---------------------------------------------------------------------------
// Entry selection
// ---------------------------------------------------------------------------

#[test]
fn entry_selection_follows_node_precedence() {
    let files = bundle(&[("z.html", "z"), ("index.html", "i"), ("a.html", "a")]);

    // 1. An explicit, sanitizable, present entry wins.
    assert_eq!(
        select_entry(&files, Some("a.html"), None).expect("entry"),
        "a.html"
    );
    assert_eq!(
        select_entry(&files, Some("./a.html"), None).expect("entry"),
        "a.html"
    );
    // 2. index.html beats the prefer-entry only when the prefer-entry is absent.
    assert_eq!(
        select_entry(&files, None, Some("z.html")).expect("entry"),
        "z.html"
    );
    assert_eq!(
        select_entry(&files, None, Some("gone.html")).expect("entry"),
        "index.html"
    );
    // 3. index.html beats first-html.
    assert_eq!(
        select_entry(&files, None, None).expect("entry"),
        "index.html"
    );

    // 4. Without index.html the FIRST html in input order wins — not the lexicographic first.
    let ordered = bundle(&[("z.html", "z"), ("a.html", "a")]);
    assert_eq!(select_entry(&ordered, None, None).expect("entry"), "z.html");

    // 5. `.htm` never auto-selects.
    let htm_only = bundle(&[("page.htm", "p"), ("style.css", "c")]);
    assert_eq!(
        select_entry(&htm_only, None, None).unwrap_err().to_string(),
        "no HTML entry found — include index.html or pass an 'entry'"
    );
    // …but it can be selected explicitly.
    assert_eq!(
        select_entry(&htm_only, Some("page.htm"), None).expect("entry"),
        "page.htm"
    );

    // 6. An entry that is not one of the files is an error naming the RAW request.
    assert_eq!(
        select_entry(&files, Some("missing.html"), None)
            .unwrap_err()
            .to_string(),
        "entry \"missing.html\" is not one of the files"
    );
    // 7. An entry that fails sanitizing is silently ignored and auto-selection runs (Node quirk).
    assert_eq!(
        select_entry(&files, Some("../evil.html"), None).expect("entry"),
        "index.html"
    );
    assert_eq!(
        select_entry(&files, Some(""), None).expect("entry"),
        "index.html"
    );
}

#[test]
fn validate_bundle_reports_the_sanitized_paths_and_entry() {
    let limits = StorageLimits::default();
    let files = bundle(&[("./pages/index.html", "i"), ("assets\\app.js", "j")]);
    let validated = validate_bundle(&files, None, None, &limits).expect("bundle");
    assert_eq!(
        validated
            .files
            .iter()
            .map(|(rel, _)| rel.as_str())
            .collect::<Vec<_>>(),
        ["pages/index.html", "assets/app.js"]
    );
    // `pages/index.html` is not the bare `index.html` key, so first-html auto-selection applies.
    assert_eq!(validated.entry, "pages/index.html");
    assert_eq!(validated.total_bytes, 2);
}

// ---------------------------------------------------------------------------
// Digest
// ---------------------------------------------------------------------------

#[test]
fn bundle_digest_is_independent_of_enumeration_order_on_disk() {
    let dir = TempDataDir::new("u07-digest");
    let root = dir.path().join("bundle");
    fs::create_dir_all(root.join("assets/deep")).expect("create bundle");
    fs::write(root.join("index.html"), "<h1>hi</h1>").expect("write");
    fs::write(root.join("assets/app.js"), "console.log(1)").expect("write");
    fs::write(root.join("assets/deep/x.css"), "body{}").expect("write");
    // An empty directory contributes nothing to the manifest.
    fs::create_dir_all(root.join("assets/empty")).expect("create empty");

    let expected = bundle_manifest_digest(&[
        ("assets/app.js", b"console.log(1)".as_slice()),
        ("assets/deep/x.css", b"body{}".as_slice()),
        ("index.html", b"<h1>hi</h1>".as_slice()),
    ]);
    assert_eq!(
        body_digest_at_path(&root, true).as_deref(),
        Some(expected.as_str())
    );

    // A single-file body is the plain SHA-256 of its bytes.
    let single = dir.path().join("single.html");
    fs::write(&single, "<h1>hi</h1>").expect("write");
    assert_eq!(
        body_digest_at_path(&single, false).as_deref(),
        Some(sha256_hex(b"<h1>hi</h1>").as_str())
    );

    // Node's `try { … } catch { return null }` cases.
    assert_eq!(
        body_digest_at_path(&dir.path().join("missing"), false),
        None
    );
    assert_eq!(body_digest_at_path(&single, true), None);
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

#[test]
fn reads_bodies_bundles_and_history_from_the_frozen_layout() {
    let dir = TempDataDir::new("u07-read");
    let artifacts = paths::artifact_dir(dir.path());
    let single = SafeArtifactId::parse("single000001").expect("id");
    let bundled = SafeArtifactId::parse("bundle000001").expect("id");
    fs::create_dir_all(&artifacts).expect("create artifact dir");

    fs::write(
        paths::single_body_path(&artifacts, &single),
        "<h1>live</h1>",
    )
    .expect("write");
    let root = paths::bundle_dir(&artifacts, &bundled);
    fs::create_dir_all(root.join("assets")).expect("create bundle");
    fs::write(root.join("index.html"), "<h1>entry</h1>").expect("write");
    fs::write(root.join("assets/app.js"), "1").expect("write");

    let history = paths::history_body_path(&artifacts, &single, 1, false);
    fs::create_dir_all(history.parent().expect("history parent")).expect("create history");
    fs::write(&history, "<h1>old</h1>").expect("write");
    let history_bundle = paths::history_body_path(&artifacts, &bundled, 1, true);
    fs::create_dir_all(&history_bundle).expect("create history bundle");
    fs::write(history_bundle.join("index.html"), "<h1>old entry</h1>").expect("write");

    let body = read_body(&artifacts, &single).expect("single body");
    assert_eq!(body.content, b"<h1>live</h1>");
    assert_eq!(body.content_type, SINGLE_BODY_CONTENT_TYPE);
    assert!(
        read_body(&artifacts, &bundled).is_none(),
        "a bundle has no <id>.html"
    );

    // No requested path falls back to the stored entry.
    let entry = read_bundle_file(&artifacts, &bundled, "index.html", None).expect("entry");
    assert_eq!(entry.content, b"<h1>entry</h1>");
    assert_eq!(entry.content_type, "text/html; charset=utf-8");

    let asset =
        read_bundle_file(&artifacts, &bundled, "index.html", Some("assets/app.js")).expect("asset");
    assert_eq!(asset.content_type, "text/javascript; charset=utf-8");
    // A normalising-but-safe request resolves to the same file.
    assert!(
        read_bundle_file(
            &artifacts,
            &bundled,
            "index.html",
            Some("./assets/../assets/app.js")
        )
        .is_some()
    );
    // Traversal, a directory, and a missing file all read as "not there".
    for rejected in [
        "../../../etc/passwd",
        "..\\..\\x",
        "assets",
        "assets/missing.js",
    ] {
        assert!(
            read_bundle_file(&artifacts, &bundled, "index.html", Some(rejected)).is_none(),
            "{rejected}"
        );
    }
    // An empty stored entry cannot address anything.
    assert!(read_bundle_file(&artifacts, &bundled, "", None).is_none());

    let revision = read_revision_body(&artifacts, &single, 1).expect("revision body");
    assert_eq!(revision.content, b"<h1>old</h1>");
    assert!(read_revision_body(&artifacts, &single, 2).is_none());

    let revision_entry = read_revision_bundle_file(&artifacts, &bundled, 1, "index.html", None)
        .expect("revision entry");
    assert_eq!(revision_entry.content, b"<h1>old entry</h1>");
    assert!(
        read_revision_bundle_file(&artifacts, &bundled, 1, "index.html", Some("../../../etc"))
            .is_none()
    );

    let tree = read_tree(&root).expect("tree");
    assert_eq!(
        tree.keys().map(String::as_str).collect::<Vec<_>>(),
        ["assets/app.js", "index.html"]
    );
    assert_eq!(
        tree.get("index.html").map(String::as_str),
        Some("<h1>entry</h1>")
    );
}
