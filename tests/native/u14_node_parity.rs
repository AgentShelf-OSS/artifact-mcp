//! Cross-runtime proof for U14's untrusted raw-response policy.
//!
//! Rust-only assertions cannot prove byte parity with the deployed bridge or JavaScript regex
//! behavior. This test drives the real `lib/artifact-http.js` through `node -e` and compares every
//! exported helper plus the download-name expression used by `lib/app.js`.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use artifact_mcp::http::artifact_response::{
    ANCHOR_BRIDGE, inject_anchor_bridge, is_html_content_type, raw_artifact_headers, strip_scripts,
};
use serde_json::{Value, json};

const REQUIRE_NODE_REFERENCE: &str = "REQUIRE_NODE_REFERENCE";

const NODE_DRIVER: &str = r#"
import(process.argv[1]).then((artifactHttp) => {
  const input = JSON.parse(process.argv[2]);
  const downloadName = (title) => {
    const name = (title || "artifact").replace(/[^\w.-]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 80) || "artifact";
    return `${name}.html`;
  };
  process.stdout.write(JSON.stringify({
    bridge: artifactHttp.ANCHOR_BRIDGE,
    injected: input.inject.map(({ content, pagePath }) => artifactHttp.injectAnchorBridge(content, { pagePath })),
    stripped: input.strip.map((content) => artifactHttp.stripScripts(content)),
    htmlTypes: input.contentTypes.map((contentType) => artifactHttp.isHtmlContentType(contentType)),
    headers: input.contentTypes.map((contentType) => artifactHttp.rawArtifactHeaders(contentType)),
    attachmentHeaders: input.titles.map((title) => artifactHttp.rawArtifactHeaders("text/html; charset=utf-8", { downloadName: downloadName(title) })),
    downloadNames: input.titles.map(downloadName)
  }));
}).catch((error) => { console.error(error); process.exit(1); });
"#;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn node_reference_available(root: &Path) -> bool {
    let required = std::env::var(REQUIRE_NODE_REFERENCE).is_ok_and(|value| value == "1");
    let unavailable = if root.join("lib/artifact-http.js").is_file() {
        match Command::new("node").arg("--version").output() {
            Ok(output) if output.status.success() => None,
            _ => Some("node is not on PATH"),
        }
    } else {
        Some("lib/artifact-http.js is missing")
    };
    match unavailable {
        None => true,
        Some(reason) => {
            assert!(
                !required,
                "{REQUIRE_NODE_REFERENCE}=1 but the Node reference is unavailable ({reason}); \
                 the U14 raw-response parity proof did not run"
            );
            eprintln!("skipping U14 Node parity proof: {reason}");
            eprintln!("set {REQUIRE_NODE_REFERENCE}=1 to make this a failure instead");
            false
        }
    }
}

fn run_node(root: &Path, request: &Value) -> Value {
    let module = format!("file://{}", root.join("lib/artifact-http.js").display());
    let output = Command::new("node")
        .current_dir(root)
        .arg("-e")
        .arg(NODE_DRIVER)
        .arg(module)
        .arg(request.to_string())
        .output()
        .expect("run Node artifact-http reference");
    assert!(
        output.status.success(),
        "Node artifact-http reference failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "Node artifact-http reference emitted invalid JSON ({error}):\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn array<'a>(response: &'a Value, field: &str) -> &'a Vec<Value> {
    response[field]
        .as_array()
        .unwrap_or_else(|| panic!("Node response has no {field} array: {response}"))
}

fn header_json(content_type: &str, attachment_name: Option<&str>) -> Value {
    let headers = raw_artifact_headers(content_type, attachment_name).expect("trusted header data");
    let mut object = serde_json::Map::new();
    for (name, value) in &headers {
        object.insert(
            name.as_str().to_owned(),
            Value::String(value.to_str().expect("ASCII response header").to_owned()),
        );
    }
    Value::Object(object)
}

#[test]
fn rust_raw_response_helpers_are_byte_identical_to_the_node_oracle() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let injections: Vec<(&str, Option<&str>)> = vec![
        (
            "<!doctype html><html><body><p>anchor me</p></body></html>",
            None,
        ),
        ("<p>no body tag</p>", None),
        ("<BODY class=x>upper</BODY\u{feff}>", None),
        (
            "<body><script>const fake='</body>'</script><!-- </body> --><p>x</p></body>",
            Some("deep/page.html"),
        ),
        ("<body>x</body>", Some("quote\"/<tag>/\u{2028}.html")),
        ("<body>x</body>", Some("cash/$&/$$/$`/$'.html")),
    ];
    let strips = [
        "<p>plain</p>",
        "<script>alert(1)</script><p>after</p>",
        "<SCRIPT type=x>one</ScRiPt><script>two</script>",
        "<scriptx>keep</script><script>drop</script>",
        "<script>unclosed",
        "a<script>b</script\u{feff}>c",
        "a<script>b</script\u{85}>c",
        "<script>first<script>nested</script>tail",
    ];
    let content_types = [
        "text/html",
        "text/html; charset=utf-8",
        "TEXT/HTML;CHARSET=UTF-8",
        "text/htmlx",
        " text/html",
        "image/svg+xml",
        "application/xml",
        "text/css; charset=utf-8",
        "image/png",
        "application/octet-stream",
    ];
    let titles = [
        "",
        "Raw Case",
        "  Quarterly 🔥 report  ",
        "---",
        "a - b",
        "résumé.2026",
        "safe_name-v2.1",
        "1234567890abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ.--------------------tail",
    ];
    let request = json!({
        "inject": injections
            .iter()
            .map(|(content, page_path)| json!({ "content": content, "pagePath": page_path }))
            .collect::<Vec<_>>(),
        "strip": strips,
        "contentTypes": content_types,
        "titles": titles,
    });
    let node = run_node(&root, &request);

    assert_eq!(node["bridge"].as_str(), Some(ANCHOR_BRIDGE));
    assert_eq!(ANCHOR_BRIDGE.len(), 12_870);

    for ((content, page_path), expected) in injections.iter().zip(array(&node, "injected")) {
        let rust = inject_anchor_bridge(content.as_bytes(), *page_path);
        assert_eq!(
            String::from_utf8(rust).expect("bridge output is UTF-8"),
            expected.as_str().expect("Node injected string"),
            "anchor injection diverged for content {content:?}, page {page_path:?}"
        );
    }
    for (content, expected) in strips.iter().zip(array(&node, "stripped")) {
        let rust = strip_scripts(content.as_bytes());
        assert_eq!(
            String::from_utf8(rust).expect("stripped output is UTF-8"),
            expected.as_str().expect("Node stripped string"),
            "script stripping diverged for {content:?}"
        );
    }
    for ((content_type, html), expected) in content_types
        .iter()
        .zip(content_types.map(is_html_content_type))
        .zip(array(&node, "htmlTypes"))
    {
        assert_eq!(
            Some(html),
            expected.as_bool(),
            "HTML content-type detection diverged for {content_type:?}"
        );
    }
    for (content_type, expected) in content_types.iter().zip(array(&node, "headers")) {
        assert_eq!(
            header_json(content_type, None),
            *expected,
            "raw headers diverged for {content_type:?}"
        );
    }
    for ((title, name), (expected_name, expected_headers)) in titles
        .iter()
        .zip(titles.map(artifact_mcp::http::artifact_response::download_name))
        .zip(
            array(&node, "downloadNames")
                .iter()
                .zip(array(&node, "attachmentHeaders")),
        )
    {
        assert_eq!(
            Some(name.as_str()),
            expected_name.as_str(),
            "title {title:?}"
        );
        assert_eq!(
            header_json("text/html; charset=utf-8", Some(&name)),
            *expected_headers,
            "attachment headers diverged for title {title:?}"
        );
    }
}
