//! Guards that every shipped browser asset actually parses.
//!
//! `assets/shell.js` shipped truncated: extracting it from `lib/portal.js` dropped the closing
//! `})();` of its IIFE, leaving the file 763 bytes short. It still rendered into the page, so every
//! server-side test and all 23 conformance cases passed — but the browser could not parse the
//! block, so no listener attached and the reaction, share and comment controls were inert while
//! navigation and download (plain links, no JS) kept working. Nothing in the suite ever executed
//! page JavaScript, so nothing caught it.
//!
//! A hand-rolled brace-balance check was tried first and was WRONG: regex literals such as
//! `/[a-z{]/` contain unbalanced braces, so it reported two perfectly valid files as broken. Use
//! the real parser instead of a proxy for it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn assets_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/assets"))
}

/// `REQUIRE_NODE_REFERENCE=1` turns "node unavailable" into a hard failure instead of a silent
/// skip, matching the policy every cross-runtime proof in this suite follows.
fn node_available() -> bool {
    match Command::new("node").arg("--version").output() {
        Ok(output) if output.status.success() => true,
        _ => {
            assert!(
                !matches!(std::env::var("REQUIRE_NODE_REFERENCE").as_deref(), Ok("1")),
                "REQUIRE_NODE_REFERENCE=1 but node is not on PATH; the browser-asset syntax proof \
                 did not run"
            );
            eprintln!("skipping browser-asset syntax proof: node is not on PATH");
            false
        }
    }
}

fn parses(path: &Path) -> Result<(), String> {
    let output = Command::new("node")
        .arg("--check")
        .arg(path)
        .output()
        .expect("run node --check");
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr)
            .lines()
            .take(3)
            .collect::<Vec<_>>()
            .join(" | "))
    }
}

#[test]
fn every_browser_asset_parses() {
    if !node_available() {
        return;
    }
    let mut broken = Vec::new();
    let mut checked = 0;
    for entry in std::fs::read_dir(assets_dir()).expect("read assets") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("js") {
            continue;
        }
        checked += 1;
        if let Err(error) = parses(&path) {
            broken.push(format!(
                "{}: {error}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
    }
    assert!(checked > 0, "no browser assets were checked");
    broken.sort();
    assert!(
        broken.is_empty(),
        "browser assets the browser cannot parse: {broken:#?}"
    );
}
