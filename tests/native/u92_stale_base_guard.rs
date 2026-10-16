//! ADR-0016: stale-base guard for full-content updates, on production adapters.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
};

use artifact_mcp::{
    artifacts::stale_guard::{self, DigestSet, Thresholds},
    config::{AppConfig, MapEnv, Secret, SeedKeys, StaleGuardConfig, StaleGuardMode},
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::u20_runtime::runtime;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "artifact-mcp-u92-stale-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create u92 temp directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ignored = std::fs::remove_dir_all(&self.0);
    }
}

struct Observer;

impl runtime::StartupObserver for Observer {
    fn stage(&self, _stage: runtime::StartupStage) {}
}

// ---------------------------------------------------------------- conformance vectors

fn vectors() -> Value {
    serde_json::from_str(include_str!("../../conformance/stale-guard-vectors.json"))
        .expect("valid stale-guard vectors")
}

fn files_digests(files: &Value) -> DigestSet {
    stale_guard::digests(files.as_array().expect("file list").iter().map(|file| {
        (
            file["path"].as_str(),
            file["body"].as_str().expect("body").as_bytes(),
        )
    }))
}

fn hex_sorted(set: &DigestSet) -> Vec<String> {
    let mut out: Vec<String> = set.iter().map(hex::encode).collect();
    out.sort();
    out
}

#[test]
fn digest_examples_match_the_reference() {
    let vectors = vectors();
    let examples = vectors["digest_examples"].as_array().expect("examples");
    assert!(!examples.is_empty());
    for example in examples {
        let set = stale_guard::digests([(
            example["path"].as_str(),
            example["body"].as_str().expect("body").as_bytes(),
        )]);
        let expected: Vec<String> = example["digests"]
            .as_array()
            .expect("digests")
            .iter()
            .map(|digest| digest.as_str().expect("hex").to_owned())
            .collect();
        assert_eq!(hex_sorted(&set), expected, "{}", example["body"]);
    }
}

#[test]
fn every_case_matches_the_reference_exactly() {
    let vectors = vectors();
    let thresholds = Thresholds {
        min_segments: vectors["min_segments"].as_u64().expect("min_segments"),
        min_share: vectors["min_share"].as_f64().expect("min_share"),
    };
    assert_eq!(thresholds, Thresholds::default());
    let cases = vectors["cases"].as_array().expect("cases");
    assert_eq!(cases.len(), 10);
    for case in cases {
        let name = case["name"].as_str().expect("name");
        let history: Vec<(u64, DigestSet)> = case["history"]
            .as_object()
            .expect("history")
            .iter()
            .map(|(revision, files)| {
                (
                    revision.parse().expect("numeric revision"),
                    files_digests(files),
                )
            })
            .collect();
        let result = stale_guard::evaluate(
            &files_digests(&case["new"]),
            &files_digests(&case["current"]),
            case["current_revision"].as_u64().expect("revision"),
            &history,
            thresholds,
        );
        let actual = json!({
            "stale": result.stale,
            "base": result.base,
            "newer_segments": result.newer_segments,
            "lost_segments": result.lost_segments,
            "message": result.message,
        });
        assert_eq!(actual, case["expect"], "{name}");
    }
}

#[test]
fn configuration_defaults_and_validation() {
    let defaults = AppConfig::defaults().stale_guard;
    assert_eq!(defaults, StaleGuardConfig::default());
    assert_eq!(defaults.mode, StaleGuardMode::Warn);
    assert_eq!(defaults.min_segments, 20);
    assert!((defaults.min_share - 0.10).abs() < f64::EPSILON);

    let parse = |pairs: &[(&str, &str)]| {
        let mut env = MapEnv::empty();
        for (key, value) in pairs {
            env = env.with(key, value);
        }
        AppConfig::from_source(&env).map(|config| config.stale_guard)
    };
    let parsed = parse(&[
        ("STALE_GUARD", "enforce"),
        ("STALE_GUARD_MIN_SEGMENTS", "5"),
        ("STALE_GUARD_MIN_SHARE", "1"),
    ])
    .expect("valid settings");
    assert_eq!(parsed.mode, StaleGuardMode::Enforce);
    assert_eq!(parsed.min_segments, 5);
    assert!((parsed.min_share - 1.0).abs() < f64::EPSILON);
    assert_eq!(
        parse(&[("STALE_GUARD", "off")]).expect("off").mode,
        StaleGuardMode::Off
    );
    // Node parity: unusable values fall back silently to the defaults.
    for bad in [
        ("STALE_GUARD", "loud"),
        ("STALE_GUARD_MIN_SEGMENTS", "0"),
        ("STALE_GUARD_MIN_SEGMENTS", "abc"),
        ("STALE_GUARD_MIN_SHARE", "0"),
        ("STALE_GUARD_MIN_SHARE", "1.5"),
        ("STALE_GUARD_MIN_SHARE", "NaN"),
        ("STALE_GUARD_MIN_SHARE", "Infinity"),
        ("STALE_GUARD_MIN_SHARE", "abc"),
        ("STALE_GUARD_MIN_SHARE", ""),
    ] {
        assert_eq!(
            parse(&[bad]).expect("fallback, not an error"),
            StaleGuardConfig::default(),
            "{bad:?}"
        );
    }
    assert_eq!(
        parse(&[("STALE_GUARD", " ENFORCE ")])
            .expect("enforce")
            .mode,
        StaleGuardMode::Enforce
    );
}

// ---------------------------------------------------------------- MCP dispatch

fn config_for(data_dir: &Path, mode: StaleGuardMode) -> AppConfig {
    let defaults = AppConfig::defaults();
    let mut ingress = defaults.ingress.clone();
    ingress.uploads_per_window = 10_000;
    ingress.mcp_per_window = 10_000;
    ingress.mutations_per_window = 10_000;
    AppConfig {
        ingress,
        data_dir: data_dir.to_path_buf(),
        audit_ledger_hmac_key: Some(Secret::new("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")),
        public_base_url: "http://conformance.test".to_owned(),
        seed_keys: SeedKeys::parse("publisher:acme:owner-secret,second:acme:second-secret"),
        stale_guard: StaleGuardConfig {
            mode,
            ..StaleGuardConfig::default()
        },
        ..defaults
    }
}

async fn call(router: &Router, key: &str, name: &str, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    });
    let response = router
        .clone()
        .oneshot(
            Request::post("/mcp")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("MCP request"),
        )
        .await
        .expect("MCP response");
    assert_eq!(response.status(), 200);
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read MCP response");
    serde_json::from_slice(&bytes).expect("MCP JSON response")
}

async fn owner(router: &Router, name: &str, arguments: Value) -> Value {
    call(router, "owner-secret", name, arguments).await
}

fn ok(response: &Value) -> &Value {
    assert!(
        response.get("error").is_none(),
        "unexpected error: {response}"
    );
    assert_ne!(
        response["result"]["isError"], true,
        "tool error: {response}"
    );
    &response["result"]["structuredContent"]
}

fn tool_error(response: &Value) -> &str {
    assert_eq!(
        response["result"]["isError"], true,
        "expected tool error: {response}"
    );
    response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool error text")
}

fn no_warnings(payload: &Value) {
    assert!(payload.get("warnings").is_none(), "{payload}");
}

/// The same synthetic document shape as `conformance/stale-guard-reference.py`.
fn doc(sections: &[(&str, u64)]) -> String {
    let mut lines = vec![
        "<!doctype html>".to_owned(),
        "<html lang=\"en\">".to_owned(),
        "<head><title>Synthetic report</title></head>".to_owned(),
        "<body>".to_owned(),
    ];
    for (name, count) in sections {
        lines.push(format!("<section id=\"{name}\">"));
        for k in 0..*count {
            lines.push(format!(
                "  <p class=\"row\">{name} line {k} value {}</p>",
                1000 + k * 37
            ));
        }
        lines.push("</section>".to_owned());
    }
    lines.push(
        "<script>const total = 42; function show() { return total * 2; }</script>".to_owned(),
    );
    lines.push("</body>".to_owned());
    lines.push("</html>".to_owned());
    lines.join("\n") + "\n"
}

fn revisions() -> [String; 4] {
    [
        doc(&[("intro", 30)]),
        doc(&[("intro", 30), ("alpha", 25)]),
        doc(&[("intro", 30), ("alpha", 25), ("beta", 25)]),
        doc(&[("intro", 30), ("alpha", 25), ("beta", 25), ("gamma", 25)]),
    ]
}

fn edit(body: &str) -> String {
    body.replace("intro line 3 value", "intro line 3 edited value")
}

const STALE_R1: &str = "Update is based on revision 1, but the artifact is at revision 4. It would discard changes from revisions 2-4. Read the current revision and apply your change to it, use restore_artifact to roll back on purpose, or pass discard_newer: true.";

/// Publish four sequential revisions of a single-file artifact and return its id.
async fn seed_single(router: &Router) -> String {
    let [r1, r2, r3, r4] = revisions();
    let created = owner(
        router,
        "publish_artifact",
        json!({ "html": r1, "title": "Report" }),
    )
    .await;
    let id = ok(&created)["id"].as_str().expect("id").to_owned();
    for body in [r2, r3, r4] {
        let updated = owner(router, "update_artifact", json!({ "id": id, "html": body })).await;
        no_warnings(ok(&updated));
    }
    id
}

async fn current_revision(router: &Router, id: &str) -> (u64, String) {
    let read = owner(
        router,
        "read_artifact",
        json!({ "id": id, "limit": 1_000_000 }),
    )
    .await;
    let read = ok(&read);
    (
        read["revision"].as_u64().expect("revision"),
        read["content"].as_str().unwrap_or_default().to_owned(),
    )
}

async fn with_runtime<F, Fut>(mode: StaleGuardMode, body: F)
where
    F: FnOnce(Router) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let temp = TempDir::new();
    runtime::run_with_bind(
        config_for(temp.path(), mode),
        Arc::new(Observer),
        |_host, _port, router| async move {
            body(router).await;
            Ok(())
        },
    )
    .await
    .expect("runtime");
}

#[tokio::test]
async fn warn_mode_writes_and_reports_stale_updates_only() {
    with_runtime(StaleGuardMode::Warn, |router| async move {
        let [r1, _r2, _r3, r4] = revisions();
        let id = seed_single(&router).await;

        // A caller that is not allowed to update gets the existing error, not a stale hint.
        let denied = call(
            &router,
            "second-secret",
            "update_artifact",
            json!({ "id": id, "html": edit(&r1) }),
        )
        .await;
        assert_eq!(
            tool_error(&denied),
            "Permission denied: this API key cannot modify this artifact"
        );

        // A mismatched expected_revision keeps the existing conflict error.
        let conflict = owner(
            &router,
            "update_artifact",
            json!({ "id": id, "html": edit(&r1), "expected_revision": 2 }),
        )
        .await;
        assert_eq!(
            tool_error(&conflict),
            "Artifact changed during update; fetch its current revision and retry"
        );

        // A non-boolean discard_newer is invalid params.
        let invalid = owner(
            &router,
            "update_artifact",
            json!({ "id": id, "html": edit(&r1), "discard_newer": "yes" }),
        )
        .await;
        assert_eq!(invalid["error"]["code"], -32602);
        assert!(
            invalid["error"]["message"]
                .as_str()
                .expect("message")
                == "Invalid arguments: discard_newer must be a boolean",
            "{invalid}"
        );
        assert_eq!(current_revision(&router, &id).await.0, 4);

        // Metadata-only updates are not checked.
        let metadata = owner(
            &router,
            "update_artifact",
            json!({ "id": id, "title": "Renamed" }),
        )
        .await;
        no_warnings(ok(&metadata));

        // A normal edit of the current body is not stale and its output is unchanged.
        let normal = owner(
            &router,
            "update_artifact",
            json!({ "id": id, "html": edit(&r4) }),
        )
        .await;
        let text = normal["result"]["content"][0]["text"]
            .as_str()
            .expect("result text")
            .to_owned();
        let normal = ok(&normal);
        no_warnings(normal);
        assert!(text.starts_with("{\"id\":"), "{text}");
        assert!(text.ends_with(&format!("\"category\":{}}}", normal["category"])), "{text}");
        assert_eq!(normal["revision"], 6);

        // discard_newer: true writes a stale body with no warning.
        let [_, r2, ..] = revisions();
        let forced = owner(
            &router,
            "update_artifact",
            json!({ "id": id, "html": r2, "discard_newer": true }),
        )
        .await;
        no_warnings(ok(&forced));
        assert_eq!(ok(&forced)["revision"], 7);
        let back = owner(&router, "restore_artifact", json!({ "id": id, "revision": 6 })).await;
        no_warnings(ok(&back));

        // A stale body over the current revision warns and still writes.
        let stale = owner(
            &router,
            "update_artifact",
            json!({ "id": id, "html": edit(&r1), "expected_revision": 8 }),
        )
        .await;
        let text = stale["result"]["content"][0]["text"]
            .as_str()
            .expect("result text")
            .to_owned();
        let stale = ok(&stale);
        assert!(text.contains(",\"warnings\":[\"Update is based"), "{text}");
        assert!(text.ends_with("\"]}"), "warnings is the last key: {text}");
        assert_eq!(stale["revision"], 9);
        let warnings = stale["warnings"].as_array().expect("warnings");
        assert_eq!(warnings.len(), 1);
        let text = warnings[0].as_str().expect("warning text");
        assert_eq!(
            text,
            "Update is based on revision 1, but the artifact is at revision 8. It would discard changes from revisions 2-8. Read the current revision and apply your change to it, use restore_artifact to roll back on purpose, or pass discard_newer: true."
        );
        assert_eq!(current_revision(&router, &id).await, (9, edit(&r1)));

        // patch_artifact and restore_artifact are not checked.
        let patched = owner(
            &router,
            "patch_artifact",
            json!({
                "id": id,
                "expected_revision": 9,
                "edits": [{ "find": "intro line 4 value", "replace": "intro line 4 patched value" }]
            }),
        )
        .await;
        no_warnings(ok(&patched));
        let restored = owner(
            &router,
            "restore_artifact",
            json!({ "id": id, "revision": 4 }),
        )
        .await;
        no_warnings(ok(&restored));
        assert_eq!(ok(&restored)["revision"], 11);
    })
    .await;
}

#[tokio::test]
async fn enforce_mode_refuses_stale_updates_and_honors_discard_newer() {
    with_runtime(StaleGuardMode::Enforce, |router| async move {
        let [r1, _r2, _r3, r4] = revisions();
        let id = seed_single(&router).await;

        let refused = owner(
            &router,
            "update_artifact",
            json!({ "id": id, "html": edit(&r1) }),
        )
        .await;
        assert_eq!(tool_error(&refused), STALE_R1);
        assert_eq!(current_revision(&router, &id).await, (4, r4.clone()));

        // A body of the wrong type gets the existing type error, not the stale text.
        let mismatch = owner(
            &router,
            "update_artifact",
            json!({ "id": id, "files": { "index.html": edit(&r1) } }),
        )
        .await;
        assert_ne!(tool_error(&mismatch), STALE_R1);
        assert_eq!(current_revision(&router, &id).await.0, 4);

        let forced = owner(
            &router,
            "update_artifact",
            json!({ "id": id, "html": edit(&r1), "discard_newer": true }),
        )
        .await;
        let forced = ok(&forced);
        no_warnings(forced);
        assert_eq!(forced["revision"], 5);
        assert_eq!(current_revision(&router, &id).await, (5, edit(&r1)));

        // discard_newer: false is a valid boolean and behaves like omission.
        let back = owner(
            &router,
            "update_artifact",
            json!({ "id": id, "html": r4, "discard_newer": false }),
        )
        .await;
        // r4 matches revision 4 exactly; revision 5 added only one segment, below the floor.
        no_warnings(ok(&back));
    })
    .await;
}

#[tokio::test]
async fn off_mode_skips_the_guard() {
    with_runtime(StaleGuardMode::Off, |router| async move {
        let [r1, ..] = revisions();
        let id = seed_single(&router).await;
        let stale = owner(
            &router,
            "update_artifact",
            json!({ "id": id, "html": edit(&r1) }),
        )
        .await;
        let stale = ok(&stale);
        no_warnings(stale);
        assert_eq!(stale["revision"], 5);
    })
    .await;
}

#[tokio::test]
async fn keyed_publish_detects_stale_single_and_bundle_bodies() {
    with_runtime(StaleGuardMode::Warn, |router| async move {
        let [r1, r2, r3, r4] = revisions();
        for body in [&r1, &r2, &r3, &r4] {
            let published = owner(
                &router,
                "publish_artifact",
                json!({ "html": body, "artifact_key": "unit/single" }),
            )
            .await;
            no_warnings(ok(&published));
        }
        let invalid = owner(
            &router,
            "publish_artifact",
            json!({ "html": edit(&r1), "artifact_key": "unit/single", "discard_newer": 1 }),
        )
        .await;
        assert_eq!(invalid["error"]["code"], -32602);

        let stale = owner(
            &router,
            "publish_artifact",
            json!({ "html": edit(&r1), "artifact_key": "unit/single" }),
        )
        .await;
        let stale = ok(&stale);
        assert_eq!(stale["created"], false);
        assert_eq!(stale["revision"], 5);
        assert_eq!(stale["warnings"], json!([STALE_R1]));

        let forced = owner(
            &router,
            "publish_artifact",
            json!({ "html": r1, "artifact_key": "unit/single", "discard_newer": true }),
        )
        .await;
        no_warnings(ok(&forced));

        let script = "const mode = 'new';\nfunction run() { return mode; }\n";
        for body in [&r1, &r2, &r3, &r4] {
            let published = owner(
                &router,
                "publish_bundle",
                json!({
                    "files": { "index.html": body, "app.js": script },
                    "artifact_key": "unit/bundle"
                }),
            )
            .await;
            no_warnings(ok(&published));
        }
        let stale = owner(
            &router,
            "publish_bundle",
            json!({
                // The new path is sanitized like the stored one, so `./index.html` still matches.
                "files": { "./index.html": edit(&r1), "app.js": script },
                "artifact_key": "unit/bundle"
            }),
        )
        .await;
        let stale = ok(&stale);
        assert_eq!(stale["revision"], 5);
        assert_eq!(stale["warnings"], json!([STALE_R1]));

        // A fresh create is never checked, and plain publishes accept the flag.
        let plain = owner(
            &router,
            "publish_artifact",
            json!({ "html": r1, "discard_newer": true }),
        )
        .await;
        no_warnings(ok(&plain));
    })
    .await;
}

#[tokio::test]
async fn keyed_publish_enforce_changes_nothing() {
    with_runtime(StaleGuardMode::Enforce, |router| async move {
        let [r1, r2, r3, r4] = revisions();
        let mut id = String::new();
        for body in [&r1, &r2, &r3, &r4] {
            let published = owner(
                &router,
                "publish_artifact",
                json!({ "html": body, "artifact_key": "unit/enforced" }),
            )
            .await;
            id = ok(&published)["id"].as_str().expect("id").to_owned();
        }
        let refused = owner(
            &router,
            "publish_artifact",
            json!({ "html": edit(&r1), "artifact_key": "unit/enforced" }),
        )
        .await;
        assert_eq!(tool_error(&refused), STALE_R1);
        assert_eq!(current_revision(&router, &id).await, (4, r4));
    })
    .await;
}
