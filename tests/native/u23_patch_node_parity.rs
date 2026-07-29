//! Direct Node/Rust byte and error parity for the PBI-037 patch engine.

use std::{path::PathBuf, process::Command};

use artifact_mcp::mcp::{protocol::OrderedJson, validation::apply_utf8_edits};
use serde_json::{Value, json};

const REQUIRE_NODE_REFERENCE: &str = "REQUIRE_NODE_REFERENCE";

const NODE_DRIVER: &str = r#"
import(process.argv[1]).then((contracts) => {
  const cases = JSON.parse(process.argv[2]);
  const results = cases.map(({ content, edits }) => {
    try {
      return { ok: true, ...contracts.applyUtf8Edits(content, edits) };
    } catch (error) {
      return { ok: false, error: String(error.message || error) };
    }
  });
  process.stdout.write(JSON.stringify(results));
}).catch((error) => { console.error(error); process.exit(1); });
"#;

fn cases() -> Value {
    json!([
        {
            "content": "before unique after",
            "edits": [{ "find": "unique", "replace": "changed" }]
        },
        {
            "content": "A🎉B---C",
            "edits": [
                { "offset": 9, "length": 1, "replace": "see" },
                { "offset": 5, "length": 1, "replace": "bee" }
            ]
        },
        {
            "content": "alpha beta alpha",
            "edits": [{ "find": "gamma", "replace": "changed" }]
        },
        {
            "content": "aaa",
            "edits": [{ "find": "aa", "replace": "x" }]
        },
        {
            "content": "A🎉B",
            "edits": [{ "offset": 2, "length": 1, "replace": "x" }]
        },
        {
            "content": "A🎉B",
            "edits": [{ "offset": 1, "length": 1, "replace": "x" }]
        },
        {
            "content": "abcdef",
            "edits": [
                { "offset": 1, "length": 3, "replace": "x" },
                { "offset": 2, "length": 1, "replace": "y" }
            ]
        },
        {
            "content": "same",
            "edits": [{ "find": "same", "replace": "same" }]
        },
        {
            "content": "ab",
            "edits": [
                { "offset": 1, "length": 0, "replace": "x" },
                { "offset": 1, "length": 0, "replace": "y" }
            ]
        }
    ])
}

fn node_available(root: &std::path::Path) -> bool {
    let unavailable = if !root.join("lib/contracts.js").is_file() {
        Some("lib/contracts.js is missing")
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
                std::env::var(REQUIRE_NODE_REFERENCE).as_deref() != Ok("1"),
                "{REQUIRE_NODE_REFERENCE}=1 but the Node patch oracle is unavailable ({reason})"
            );
            eprintln!("skipping U23 Node patch parity proof: {reason}");
            false
        }
    }
}

fn node_results(root: &std::path::Path, fixtures: &Value) -> Value {
    let module = format!("file://{}", root.join("lib/contracts.js").display());
    let output = Command::new("node")
        .current_dir(root)
        .arg("-e")
        .arg(NODE_DRIVER)
        .arg(module)
        .arg(fixtures.to_string())
        .output()
        .expect("run Node patch oracle");
    assert!(
        output.status.success(),
        "Node patch oracle failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Node patch oracle emitted JSON")
}

fn rust_results(fixtures: &Value) -> Value {
    let results = fixtures
        .as_array()
        .expect("fixture array")
        .iter()
        .map(|fixture| {
            let content = fixture["content"].as_str().expect("fixture content");
            let edits: OrderedJson =
                serde_json::from_value(fixture["edits"].clone()).expect("fixture edits");
            match apply_utf8_edits(content.as_bytes(), &edits) {
                Ok(patched) => json!({
                    "ok": true,
                    "content": String::from_utf8(patched.clone()).expect("valid patched UTF-8"),
                    "bytes_before": content.len(),
                    "bytes_after": patched.len()
                }),
                Err(error) => json!({ "ok": false, "error": error.to_string() }),
            }
        })
        .collect::<Vec<_>>();
    Value::Array(results)
}

#[test]
fn node_and_rust_patch_bytes_and_errors_match_exactly() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if !node_available(&root) {
        return;
    }
    let fixtures = cases();
    assert_eq!(rust_results(&fixtures), node_results(&root, &fixtures));
}
